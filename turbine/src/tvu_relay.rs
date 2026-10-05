//! Snapshot and shred handling for a TVU relay.
//!
//! A relay is an extra TVU address for one staked validator. It verifies and
//! retransmits as that validator, and forwards the original shred home so the
//! validator can replay it without fanning out a second time.

use {
    crate::{
        cluster_nodes::{ClusterNodes, DATA_PLANE_FANOUT, RelayNodeInput},
        retransmit_stage::RetransmitStage,
    },
    agave_feature_set as feature_set,
    serde::{Deserialize, Serialize},
    solana_clock::{Epoch, Slot},
    solana_cluster_type::ClusterType,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::Protocol, tvu_rtt::TvuRttCache},
    solana_keypair::Keypair,
    solana_ledger::{
        leader_schedule_cache::LeaderScheduleCache,
        shred::{self, wire::resign_shred},
    },
    solana_net_utils::SocketAddrSpace,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_signature::Signature,
    solana_signer::Signer,
    std::{
        io::{Read, Write},
        net::{SocketAddr, TcpListener, TcpStream},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{Builder, JoinHandle},
        time::Duration,
    },
};

const SHRED_VERSION_OFFSET: usize = 77;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RelayNodeView {
    pub pubkey: Pubkey,
    pub stake: u64,
    pub wallclock: u64,
    pub tvu: Option<SocketAddr>,
    pub relays: Vec<SocketAddr>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EpochLeaders {
    pub first_slot: Slot,
    pub leaders: Vec<Pubkey>,
}

/// What the relay needs instead of a bank.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RelaySnapshot {
    pub identity: Pubkey,
    pub root: Slot,
    pub shred_version: u16,
    pub cluster_type: ClusterType,
    pub use_chacha8: bool,
    pub verify_retransmitter: bool,
    pub epochs: Vec<EpochLeaders>,
    pub nodes: Vec<RelayNodeView>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum RelayFeedMessage {
    Snapshot(RelaySnapshot),
    Root(Slot),
}

pub struct RelayView {
    pub snapshot: RelaySnapshot,
    pub cluster_nodes: ClusterNodes<RetransmitStage>,
}

impl RelayView {
    pub fn new(snapshot: RelaySnapshot) -> Self {
        let cluster_nodes = cluster_nodes_from_snapshot(&snapshot);
        Self {
            snapshot,
            cluster_nodes,
        }
    }

    pub fn set_root(&mut self, root: Slot) {
        self.snapshot.root = root;
    }
}

fn cluster_nodes_from_snapshot(snapshot: &RelaySnapshot) -> ClusterNodes<RetransmitStage> {
    let dedup_tvu = match snapshot.cluster_type {
        ClusterType::Development => false,
        ClusterType::Devnet | ClusterType::Testnet | ClusterType::MainnetBeta => true,
    };
    let inputs = snapshot
        .nodes
        .iter()
        .map(|node| RelayNodeInput {
            pubkey: node.pubkey,
            stake: node.stake,
            wallclock: node.wallclock,
            tvu: node.tvu,
            relays: node.relays.clone(),
        })
        .collect();
    ClusterNodes::from_relay_nodes(snapshot.identity, inputs, snapshot.use_chacha8, dedup_tvu)
}

pub fn build_relay_snapshot(
    identity: &Pubkey,
    root_bank: &Bank,
    leader_schedule_cache: &LeaderScheduleCache,
    cluster_info: &ClusterInfo,
) -> RelaySnapshot {
    let root = root_bank.slot();
    let epoch_schedule = root_bank.epoch_schedule();
    let epoch = epoch_schedule.get_epoch(root);
    let mut epochs = Vec::new();
    for epoch in epoch..=epoch.saturating_add(1) {
        let first_slot = epoch_schedule.get_first_slot_in_epoch(epoch);
        let slots = epoch_schedule.get_slots_in_epoch(epoch);
        let mut leaders = Vec::with_capacity(slots as usize);
        let mut missing = false;
        for index in 0..slots {
            let Some(leader) =
                leader_schedule_cache.slot_leader_at(first_slot + index, Some(root_bank))
            else {
                missing = true;
                break;
            };
            leaders.push(leader.id);
        }
        if !missing && !leaders.is_empty() {
            epochs.push(EpochLeaders {
                first_slot,
                leaders,
            });
        }
    }

    let mut nodes = Vec::new();
    let mut push_contact = |node: &solana_gossip::contact_info::ContactInfo, stake: u64| {
        nodes.push(RelayNodeView {
            pubkey: *node.pubkey(),
            stake,
            wallclock: node.wallclock(),
            tvu: node.tvu(Protocol::UDP),
            relays: node.tvu_relays().to_vec(),
        });
    };

    let stakes = root_bank
        .epoch_staked_nodes(epoch)
        .unwrap_or_else(|| Arc::new(std::collections::HashMap::new()));
    let me = cluster_info.my_contact_info();
    push_contact(&me, stakes.get(me.pubkey()).copied().unwrap_or(0));
    for node in cluster_info.tvu_peers(|node| node.clone()) {
        push_contact(&node, stakes.get(node.pubkey()).copied().unwrap_or(0));
    }
    let seen: std::collections::HashSet<Pubkey> = nodes.iter().map(|node| node.pubkey).collect();
    for (pubkey, stake) in stakes.iter() {
        if *stake > 0 && !seen.contains(pubkey) {
            nodes.push(RelayNodeView {
                pubkey: *pubkey,
                stake: *stake,
                wallclock: 0,
                tvu: None,
                relays: Vec::new(),
            });
        }
    }

    RelaySnapshot {
        identity: *identity,
        root,
        shred_version: cluster_info.my_shred_version(),
        cluster_type: root_bank.cluster_type(),
        use_chacha8: solana_ledger::shred::filter::check_feature_activation_from_bank(
            &feature_set::switch_to_chacha8_turbine::id(),
            root,
            root_bank,
        ),
        verify_retransmitter: solana_ledger::shred::filter::check_feature_activation_from_bank(
            &feature_set::verify_retransmitter_signature::id(),
            root,
            root_bank,
        ),
        epochs,
        nodes,
    }
}

pub fn spawn_relay_feed(
    bind: SocketAddr,
    cluster_info: Arc<ClusterInfo>,
    bank_forks: Arc<std::sync::RwLock<solana_runtime::bank_forks::BankForks>>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    exit: Arc<AtomicBool>,
) -> JoinHandle<()> {
    Builder::new()
        .name("solTvuRelayFd".to_string())
        .spawn(move || {
            let listener = match TcpListener::bind(bind) {
                Ok(listener) => listener,
                Err(err) => {
                    error!("tvu relay feed bind {bind}: {err}");
                    return;
                }
            };
            let _ = listener.set_nonblocking(true);
            while !exit.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nodelay(true);
                        serve_feed_client(
                            stream,
                            &cluster_info,
                            &bank_forks,
                            &leader_schedule_cache,
                            &exit,
                        );
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(err) => {
                        warn!("tvu relay feed accept: {err}");
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        })
        .unwrap()
}

fn serve_feed_client(
    mut stream: TcpStream,
    cluster_info: &ClusterInfo,
    bank_forks: &std::sync::RwLock<solana_runtime::bank_forks::BankForks>,
    leader_schedule_cache: &LeaderScheduleCache,
    exit: &AtomicBool,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let mut last_epoch: Option<Epoch> = None;
    while !exit.load(Ordering::Relaxed) {
        let (root_bank, identity) = {
            let bank_forks = bank_forks.read().unwrap();
            (bank_forks.root_bank(), cluster_info.id())
        };
        let epoch = root_bank.epoch();
        if last_epoch != Some(epoch) {
            let snapshot =
                build_relay_snapshot(&identity, &root_bank, leader_schedule_cache, cluster_info);
            if write_feed_message(&mut stream, &RelayFeedMessage::Snapshot(snapshot)).is_err() {
                return;
            }
            last_epoch = Some(epoch);
        } else if write_feed_message(&mut stream, &RelayFeedMessage::Root(root_bank.slot()))
            .is_err()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

pub fn write_feed_message(
    stream: &mut impl Write,
    message: &RelayFeedMessage,
) -> std::io::Result<()> {
    let bytes = bincode::serialize(message).map_err(std::io::Error::other)?;
    let len = u32::try_from(bytes.len()).map_err(std::io::Error::other)?;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()
}

pub fn read_feed_message(stream: &mut impl Read) -> std::io::Result<RelayFeedMessage> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 64 * 1024 * 1024 {
        return Err(std::io::Error::other("tvu relay feed message too large"));
    }
    let mut bytes = vec![0u8; len];
    stream.read_exact(&mut bytes)?;
    bincode::deserialize(&bytes).map_err(std::io::Error::other)
}

pub struct RelayForward {
    /// Original shred, still signed by the upstream turbine parent.
    pub to_validator: Vec<u8>,
    /// Resigned shred and the children it should be sent to.
    pub to_children: Option<(Vec<u8>, Vec<SocketAddr>)>,
}

/// Verify `shred` as `view.snapshot.identity` and, when it belongs on that
/// turbine seat, resign it for downstream children.
pub fn handle_shred(
    shred: &[u8],
    view: &RelayView,
    rtt: &TvuRttCache,
    keypair: &Keypair,
) -> Option<RelayForward> {
    if keypair.pubkey() != view.snapshot.identity {
        return None;
    }
    let my_stake = view
        .snapshot
        .nodes
        .iter()
        .find(|node| node.pubkey == view.snapshot.identity)
        .map(|node| node.stake)
        .unwrap_or(0);
    if my_stake == 0 {
        return None;
    }
    if shred.len() < SHRED_VERSION_OFFSET + 2 {
        return None;
    }
    let version = u16::from_le_bytes(
        shred[SHRED_VERSION_OFFSET..SHRED_VERSION_OFFSET + 2]
            .try_into()
            .ok()?,
    );
    if version != view.snapshot.shred_version && view.snapshot.shred_version != 0 {
        return None;
    }
    let slot = shred::layout::get_slot(shred)?;
    if slot <= view.snapshot.root {
        return None;
    }
    let leader = leader_at(&view.snapshot, slot)?;
    if leader == view.snapshot.identity {
        return None;
    }
    let signature = Signature::try_from(shred.get(..64)?).ok()?;
    let merkle_root = shred::layout::get_merkle_root(shred)?;
    if !signature.verify(leader.as_ref(), merkle_root.as_ref()) {
        return None;
    }
    let shred_id = shred::layout::get_shred_id(shred)?;
    let parent = view
        .cluster_nodes
        .get_retransmit_parent(&leader, &shred_id, DATA_PLANE_FANOUT)
        .ok()?;
    let signed_variant = shred::layout::is_retransmitter_signed_variant(shred).ok()?;
    if signed_variant && let Some(parent) = parent {
        let parent_sig = shred::layout::get_retransmitter_signature(shred).ok()?;
        let parent_ok = parent_sig.verify(parent.as_ref(), merkle_root.as_ref());
        if view.snapshot.verify_retransmitter && !parent_ok {
            return None;
        }
    }

    let mut resigned = shred.to_vec();
    if signed_variant && resign_shred(&mut resigned, keypair).is_err() {
        return None;
    }
    let (_distance, children) = view
        .cluster_nodes
        .get_retransmit_addrs(
            &leader,
            &shred_id,
            DATA_PLANE_FANOUT,
            &SocketAddrSpace::Unspecified,
            rtt,
        )
        .ok()?;
    Some(RelayForward {
        to_validator: shred.to_vec(),
        to_children: (!children.is_empty()).then_some((resigned, children)),
    })
}

fn leader_at(snapshot: &RelaySnapshot, slot: Slot) -> Option<Pubkey> {
    for epoch in &snapshot.epochs {
        let end = epoch.first_slot.saturating_add(epoch.leaders.len() as u64);
        if (epoch.first_slot..end).contains(&slot) {
            return Some(epoch.leaders[(slot - epoch.first_slot) as usize]);
        }
    }
    None
}

/// Home validator should not fan a shred out again when it arrived from one
/// of its own relays. The relay already retransmitted.
#[must_use]
pub fn suppress_retransmit(src: SocketAddr, relays: &[SocketAddr]) -> bool {
    relays.contains(&src)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppress_only_own_relays() {
        let relay = SocketAddr::from(([1, 2, 3, 4], 9000));
        let other = SocketAddr::from(([5, 6, 7, 8], 9000));
        assert!(suppress_retransmit(relay, &[relay]));
        assert!(!suppress_retransmit(other, &[relay]));
    }

    #[test]
    fn leader_lookup_uses_epoch_table() {
        let leader = Pubkey::new_unique();
        let snapshot = RelaySnapshot {
            identity: Pubkey::new_unique(),
            root: 0,
            shred_version: 0,
            cluster_type: ClusterType::Development,
            use_chacha8: false,
            verify_retransmitter: false,
            epochs: vec![EpochLeaders {
                first_slot: 10,
                leaders: vec![leader, Pubkey::new_unique()],
            }],
            nodes: Vec::new(),
        };
        assert_eq!(leader_at(&snapshot, 10), Some(leader));
        assert_eq!(leader_at(&snapshot, 9), None);
    }

    #[test]
    fn garbage_shred_is_not_forwarded() {
        let identity = Keypair::new();
        let snapshot = RelaySnapshot {
            identity: identity.pubkey(),
            root: 0,
            shred_version: 0,
            cluster_type: ClusterType::Development,
            use_chacha8: false,
            verify_retransmitter: true,
            epochs: Vec::new(),
            nodes: vec![RelayNodeView {
                pubkey: identity.pubkey(),
                stake: 1,
                wallclock: 1,
                tvu: Some(SocketAddr::from(([127, 0, 0, 1], 8001))),
                relays: Vec::new(),
            }],
        };
        let view = RelayView::new(snapshot);
        let rtt = TvuRttCache::default();
        assert!(handle_shred(&[0u8; 128], &view, &rtt, &identity).is_none());
    }
}
