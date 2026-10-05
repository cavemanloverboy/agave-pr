//! RTT samples used to choose among a node's TVU destinations.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::RwLock,
    time::{Duration, Instant},
};

const MAX_SAMPLE_AGE: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct TvuRttCache {
    samples: RwLock<HashMap<SocketAddr, (Duration, Instant)>>,
}

impl TvuRttCache {
    pub fn record(&self, addr: SocketAddr, rtt: Duration) {
        self.samples
            .write()
            .unwrap()
            .insert(addr, (rtt, Instant::now()));
    }

    /// Lowest fresh RTT among `primary` and `relays`.
    ///
    /// With no fresh samples, returns `primary` so senders keep today's
    /// single-address behavior until pings land.
    #[must_use]
    pub fn select(&self, primary: Option<SocketAddr>, relays: &[SocketAddr]) -> Option<SocketAddr> {
        let samples = self.samples.read().unwrap();
        let now = Instant::now();
        let mut best: Option<(SocketAddr, Duration)> = None;
        let mut consider = |addr: SocketAddr| {
            if addr.is_ipv6() {
                return;
            }
            let Some((rtt, at)) = samples.get(&addr) else {
                return;
            };
            if now.saturating_duration_since(*at) > MAX_SAMPLE_AGE {
                return;
            }
            if best.is_none_or(|(_, best_rtt)| *rtt < best_rtt) {
                best = Some((addr, *rtt));
            }
        };
        if let Some(primary) = primary {
            consider(primary);
        }
        for relay in relays {
            consider(*relay);
        }
        best.map(|(addr, _)| addr)
            .or_else(|| primary.filter(|addr| !addr.is_ipv6()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(oct: u8) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, oct], 8001))
    }

    #[test]
    fn falls_back_to_primary_without_samples() {
        let cache = TvuRttCache::default();
        let primary = addr(1);
        assert_eq!(cache.select(Some(primary), &[addr(2)]), Some(primary));
        assert_eq!(cache.select(None, &[addr(2)]), None);
    }

    #[test]
    fn picks_lowest_ping() {
        let cache = TvuRttCache::default();
        let primary = addr(1);
        let relay = addr(2);
        cache.record(primary, Duration::from_millis(40));
        cache.record(relay, Duration::from_millis(5));
        assert_eq!(cache.select(Some(primary), &[relay]), Some(relay));
        cache.record(relay, Duration::from_millis(80));
        assert_eq!(cache.select(Some(primary), &[relay]), Some(primary));
    }
}
