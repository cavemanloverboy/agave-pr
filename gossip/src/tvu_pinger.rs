//! Background RTT probes toward remote TVU sockets.

use {
    crate::{cluster_info::ClusterInfo, tvu_ping},
    std::{
        collections::HashMap,
        net::{SocketAddr, UdpSocket},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

const PROBE_INTERVAL: Duration = Duration::from_secs(2);

pub fn spawn_tvu_pinger(cluster_info: Arc<ClusterInfo>, exit: Arc<AtomicBool>) -> JoinHandle<()> {
    Builder::new()
        .name("solTvuPing".to_string())
        .spawn(move || {
            let sock = match UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))) {
                Ok(sock) => sock,
                Err(err) => {
                    log::warn!("tvu pinger failed to bind: {err}");
                    return;
                }
            };
            if let Err(err) = sock.set_nonblocking(true) {
                log::warn!("tvu pinger set_nonblocking: {err}");
                return;
            }
            let mut token = 0u64;
            let mut inflight: HashMap<u64, (SocketAddr, Instant)> = HashMap::new();
            let mut last_round = Instant::now()
                .checked_sub(PROBE_INTERVAL)
                .unwrap_or_else(Instant::now);
            let mut buf = [0u8; 64];
            while !exit.load(Ordering::Relaxed) {
                while let Ok((n, src)) = sock.recv_from(&mut buf) {
                    let Some(got) = tvu_ping::decode(&buf[..n]) else {
                        continue;
                    };
                    let Some((dst, sent)) = inflight.remove(&got) else {
                        continue;
                    };
                    if src == dst {
                        cluster_info.record_tvu_rtt(dst, sent.elapsed());
                    }
                }
                if last_round.elapsed() >= PROBE_INTERVAL {
                    inflight.clear();
                    for addr in cluster_info.remote_tvu_destinations() {
                        token = token.wrapping_add(1);
                        let mut pkt = [0u8; tvu_ping::TVU_PING_LEN];
                        tvu_ping::encode(token, &mut pkt);
                        if sock.send_to(&pkt, addr).is_ok() {
                            inflight.insert(token, (addr, Instant::now()));
                        }
                    }
                    last_round = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
        .unwrap()
}
