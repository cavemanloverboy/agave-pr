//! Shared helpers for collecting and buffering per-slot account deltas along the
//! live parent chain of confirmed banks.
//!
//! Confirmed banks form a single non-reverting chain, and their parent links are
//! still intact (banks are only squashed when rooted, and roots trail
//! confirmation), so walking `Bank::parent()` between two confirmed slots yields
//! a complete, ordered patch of every account write in between.

use {
    log::{info, warn},
    solana_account::AccountSharedData,
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    std::{
        collections::VecDeque,
        time::{Duration, Instant},
    },
};

pub(crate) type AccountDelta = (Pubkey, AccountSharedData);
pub(crate) type PendingSlotDelta = (Slot, Vec<AccountDelta>);

/// Warm-up window before scheduling a full rebuild.
const WARMUP_WINDOW: Duration = Duration::from_secs(10);
/// Max confirmed-slot rate that counts as steady (real time is ~2.5 slots/s;
/// backlog replay runs at 10-50 slots/s).
const MAX_STEADY_SLOTS_PER_SECOND: f64 = 5.0;

/// Wall-clock rate gate for scheduling full rebuilds.
///
/// A rebuild's pending buffer assumes roots never overtake the last buffered
/// slot between hook calls; that only holds while confirmed slots advance at
/// the cluster's real-time rate. While the node replays backlog, confirmed
/// slots advance at replay speed instead, and roots (which trail replay, not
/// wall clock) sweep past any buffer anchor — a rebuild scheduled then is
/// doomed to be invalidated. The rate must be measured against wall clock
/// over a window: per-call slot steps look small during a healthy catch-up
/// because the hook loop iterates every ~200ms (observed live: warming gated
/// on ≤8-slot steps passed 5 minutes into a ~1000-slot backlog and the
/// resulting rebuild was discarded).
pub(crate) struct WarmupWindow {
    window_start: Instant,
    window_start_slot: Slot,
}

impl WarmupWindow {
    pub(crate) fn new(slot: Slot) -> Self {
        Self {
            window_start: Instant::now(),
            window_start_slot: slot,
        }
    }

    /// Observe the next confirmed slot. Returns true once a full
    /// [`WARMUP_WINDOW`] has elapsed at steady (real-time) confirmed
    /// progression — i.e. it is safe to schedule a rebuild. Resets the window
    /// whenever progression is faster than real time (backlog replay).
    pub(crate) fn observe(&mut self, label: &str, slot: Slot) -> bool {
        let elapsed = self.window_start.elapsed();
        if elapsed < WARMUP_WINDOW {
            return false;
        }
        let rate = slot.saturating_sub(self.window_start_slot) as f64 / elapsed.as_secs_f64();
        if rate <= MAX_STEADY_SLOTS_PER_SECOND {
            true
        } else {
            info!(
                "{label}: confirmed slots advancing at {rate:.1} slots/s (backlog replay); \
                 deferring full rebuild"
            );
            self.window_start = Instant::now();
            self.window_start_slot = slot;
            false
        }
    }
}

/// Ascending `(slot, accounts-modified-since-parent)` from `from_slot` (exclusive)
/// to `bank` (inclusive), walking the live parent chain.
pub(crate) fn collect_parent_chain_deltas(
    from_slot: Slot,
    bank: &Bank,
) -> Result<Vec<PendingSlotDelta>, &'static str> {
    let to_slot = bank.slot();
    if from_slot == to_slot {
        return Ok(vec![]);
    }
    if from_slot > to_slot {
        return Err("cache ahead of confirmed bank");
    }

    let mut descending = Vec::new();
    let mut bank_arc: Option<std::sync::Arc<Bank>> = None;
    loop {
        let current: &Bank = match &bank_arc {
            None => bank,
            Some(bank) => bank,
        };
        let slot = current.slot();
        if slot == from_slot {
            descending.reverse();
            return Ok(descending);
        }
        if slot < from_slot {
            return Err("confirmed bank not descendant of cache slot");
        }
        descending.push((slot, current.get_all_accounts_modified_since_parent()));
        match current.parent() {
            Some(parent) => bank_arc = Some(parent),
            None => return Err("parent chain broken before cache slot"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window_started_secs_ago(secs: u64, slot: Slot) -> WarmupWindow {
        WarmupWindow {
            window_start: Instant::now() - Duration::from_secs(secs),
            window_start_slot: slot,
        }
    }

    #[test]
    fn warmup_window_gates_on_wall_clock_rate() {
        // Window not yet elapsed: not ready even at zero progression.
        let mut fresh = WarmupWindow::new(1_000);
        assert!(!fresh.observe("test", 1_001));

        // Steady state: ~2.3 slots/s over 11s → ready.
        let mut steady = window_started_secs_ago(11, 1_000);
        assert!(steady.observe("test", 1_025));

        // Backlog replay: ~27 slots/s over 11s → window resets, not ready,
        // and the reset anchors at the observed slot.
        let mut catching_up = window_started_secs_ago(11, 1_000);
        assert!(!catching_up.observe("test", 1_300));
        assert_eq!(catching_up.window_start_slot, 1_300);
    }
}

/// Buffer per-slot deltas from `expected_from` (exclusive) to `bank` (inclusive)
/// onto `pending`. On a gap, a broken parent chain, or overflow past `cap`,
/// clears `pending` and sets `invalidated` so the in-flight rebuild is discarded.
pub(crate) fn buffer_chain_deltas(
    label: &str,
    bank: &Bank,
    expected_from: Slot,
    pending: &mut VecDeque<PendingSlotDelta>,
    invalidated: &mut bool,
    cap: usize,
) {
    if *invalidated {
        return;
    }
    let to_slot = bank.slot();
    if to_slot == expected_from {
        return;
    }
    if to_slot < expected_from {
        warn!(
            "{label}: pending delta gap at slot={to_slot} expected_from={expected_from}; \
             will discard in-flight rebuild"
        );
        pending.clear();
        *invalidated = true;
        return;
    }

    match collect_parent_chain_deltas(expected_from, bank) {
        Ok(deltas) => {
            for item in deltas {
                pending.push_back(item);
            }
            if pending.len() > cap {
                warn!(
                    "{label}: pending update queue full (cap={cap}) at tip_slot={to_slot}; \
                     will discard in-flight rebuild"
                );
                pending.clear();
                *invalidated = true;
            }
        }
        Err(reason) => {
            warn!(
                "{label}: {reason} at tip_slot={to_slot} expected_from={expected_from}; \
                 will discard in-flight rebuild"
            );
            pending.clear();
            *invalidated = true;
        }
    }
}
