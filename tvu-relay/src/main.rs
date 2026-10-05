//! Receives turbine shreds for one validator, retransmits them, and forwards
//! the original shred to the validator's TVU.
//!
//! The process is not a validator. Leader schedule, stakes, and peer TVU
//! addresses arrive on a TCP feed from the validator (`--tvu-relay-feed-address`).

use {
    agave_logger,
    clap::{App, Arg, crate_description, crate_name, value_t_or_exit},
    solana_clap_utils::input_parsers::keypair_of,
    solana_gossip::{tvu_ping, tvu_rtt::TvuRttCache},
    solana_signer::Signer,
    solana_turbine::tvu_relay::{RelayFeedMessage, RelayView, handle_shred, read_feed_message},
    std::{
        collections::HashMap,
        net::{SocketAddr, TcpStream, UdpSocket},
        sync::{Arc, RwLock},
        thread,
        time::{Duration, Instant},
    },
};

fn main() {
    agave_logger::setup_with_default("agave_tvu_relay=info");
    let matches = App::new(crate_name!())
        .about(crate_description!())
        .version(solana_version::version!())
        .arg(
            Arg::with_name("identity")
                .long("identity")
                .value_name("KEYPAIR")
                .takes_value(true)
                .required(true)
                .help("Validator identity keypair. Used only to resign shreds."),
        )
        .arg(
            Arg::with_name("bind")
                .long("bind")
                .value_name("HOST:PORT")
                .takes_value(true)
                .required(true)
                .validator(solana_net_utils::is_host_port)
                .help(
                    "TVU address to bind. Publish this with the validator's --tvu-relay-address.",
                ),
        )
        .arg(
            Arg::with_name("forward")
                .long("forward-tvu")
                .value_name("HOST:PORT")
                .takes_value(true)
                .required(true)
                .validator(solana_net_utils::is_host_port)
                .help("Validator TVU address that should receive original shreds."),
        )
        .arg(
            Arg::with_name("feed")
                .long("feed")
                .value_name("HOST:PORT")
                .takes_value(true)
                .required(true)
                .validator(solana_net_utils::is_host_port)
                .help("Validator --tvu-relay-feed-address."),
        )
        .get_matches();

    let identity = keypair_of(&matches, "identity").expect("identity keypair");
    let bind: SocketAddr = value_t_or_exit!(matches, "bind", SocketAddr);
    let forward: SocketAddr = value_t_or_exit!(matches, "forward", SocketAddr);
    let feed: SocketAddr = value_t_or_exit!(matches, "feed", SocketAddr);

    let view = Arc::new(RwLock::new(None::<RelayView>));
    let rtt = Arc::new(TvuRttCache::default());
    {
        let view = Arc::clone(&view);
        thread::Builder::new()
            .name("solTvuRlyFeed".to_string())
            .spawn(move || feed_loop(feed, view))
            .unwrap();
    }

    let socket = UdpSocket::bind(bind).unwrap_or_else(|err| {
        eprintln!("bind {bind}: {err}");
        std::process::exit(1);
    });
    socket.set_read_timeout(Some(Duration::from_secs(1))).ok();
    log::info!(
        "agave-tvu-relay listening on {bind} for {}",
        identity.pubkey()
    );

    let mut buf = [0u8; 2048];
    let mut seen: HashMap<(u64, u32), u8> = HashMap::new();
    let mut last_ping = Instant::now()
        .checked_sub(Duration::from_secs(2))
        .unwrap_or_else(Instant::now);
    let mut token = 0u64;
    let mut inflight: HashMap<u64, (SocketAddr, Instant)> = HashMap::new();
    loop {
        while let Ok((n, src)) = socket.recv_from(&mut buf) {
            if let Some(got) = tvu_ping::decode(&buf[..n]) {
                if let Some((dst, sent)) = inflight.remove(&got) {
                    if src == dst {
                        rtt.record(dst, sent.elapsed());
                    }
                } else {
                    let mut pkt = [0u8; tvu_ping::TVU_PING_LEN];
                    tvu_ping::encode(got, &mut pkt);
                    let _ = socket.send_to(&pkt, src);
                }
                continue;
            }
            let guard = view.read().unwrap();
            let Some(view) = guard.as_ref() else {
                continue;
            };
            let Some(slot) = solana_ledger_slot(&buf[..n]) else {
                continue;
            };
            let index = u32::from_le_bytes(
                buf.get(73..77)
                    .unwrap_or(&[0; 4])
                    .try_into()
                    .unwrap_or([0; 4]),
            );
            let key = (slot, index);
            let count = seen.entry(key).or_insert(0);
            if *count >= 2 {
                continue;
            }
            let Some(forwarded) = handle_shred(&buf[..n], view, &rtt, &identity) else {
                continue;
            };
            *count = count.saturating_add(1);
            let _ = socket.send_to(&forwarded.to_validator, forward);
            if let Some((payload, children)) = forwarded.to_children {
                for child in children {
                    let _ = socket.send_to(&payload, child);
                }
            }
        }
        if last_ping.elapsed() >= Duration::from_secs(2) {
            if let Some(view) = view.read().unwrap().as_ref() {
                inflight.clear();
                for node in &view.snapshot.nodes {
                    if node.pubkey == identity.pubkey() {
                        continue;
                    }
                    let mut dests = Vec::new();
                    if let Some(tvu) = node.tvu {
                        dests.push(tvu);
                    }
                    dests.extend(node.relays.iter().copied());
                    for addr in dests {
                        token = token.wrapping_add(1);
                        let mut pkt = [0u8; tvu_ping::TVU_PING_LEN];
                        tvu_ping::encode(token, &mut pkt);
                        if socket.send_to(&pkt, addr).is_ok() {
                            inflight.insert(token, (addr, Instant::now()));
                        }
                    }
                }
            }
            last_ping = Instant::now();
            seen.retain(|(slot, _), _| {
                view.read()
                    .unwrap()
                    .as_ref()
                    .is_none_or(|view| *slot + 32 > view.snapshot.root)
            });
        }
    }
}

fn feed_loop(feed: SocketAddr, view: Arc<RwLock<Option<RelayView>>>) {
    loop {
        match TcpStream::connect_timeout(&feed, Duration::from_secs(2)) {
            Ok(mut stream) => {
                log::info!("connected to tvu relay feed {feed}");
                let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                loop {
                    match read_feed_message(&mut stream) {
                        Ok(RelayFeedMessage::Snapshot(snapshot)) => {
                            log::info!(
                                "snapshot root {} identity {} nodes {}",
                                snapshot.root,
                                snapshot.identity,
                                snapshot.nodes.len()
                            );
                            *view.write().unwrap() = Some(RelayView::new(snapshot));
                        }
                        Ok(RelayFeedMessage::Root(root)) => {
                            if let Some(view) = view.write().unwrap().as_mut() {
                                view.set_root(root);
                            }
                        }
                        Err(err) => {
                            log::warn!("tvu relay feed: {err}");
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                log::warn!("tvu relay feed connect {feed}: {err}");
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn solana_ledger_slot(shred: &[u8]) -> Option<u64> {
    let bytes: [u8; 8] = shred.get(65..73)?.try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}
