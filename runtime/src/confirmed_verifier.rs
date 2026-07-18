//! Callbacks invoked when a bank becomes optimistically confirmed.
//!
//! Confirmed is the verification tier of choice: processed/frozen banks can
//! still be abandoned on a minor fork, while roots trail confirmation and are
//! already squashed (parent links severed, accounts-index roots advanced) by
//! the time rooting is observable. An optimistically confirmed bank is frozen,
//! will not revert, and still has its parent chain intact, so verifiers can
//! walk `bank.parent()` for per-slot deltas without being fork-aware.
//!
//! Callbacks run on the `ClusterInfoVoteListener` vote-processing thread (off
//! the replay path) — the thread that detects optimistic confirmation from
//! gossip + replay votes, so the hook fires regardless of whether RPC services
//! are enabled. Calls are strictly ascending in slot, for the highest confirmed
//! bank frozen locally at each detection; intermediate confirmed slots are
//! skipped, so verifiers must tolerate gaps by walking parent chains from
//! their last verified slot.
//!
//! NOTE: after the Alpenglow migration completes, tower-BFT optimistic
//! confirmation stops being reported (`should_report_commitment_or_root`
//! becomes false) and votor reports OC at root time instead — this hook must
//! then move to votor's rooting path (`votor::root_utils::set_root`).

use {
    crate::bank::Bank,
    std::sync::{Arc, OnceLock, RwLock},
};

type ConfirmedVerifierFn = Box<dyn Fn(&Arc<Bank>) + Send + Sync>;

static CONFIRMED_VERIFIERS: OnceLock<RwLock<Vec<ConfirmedVerifierFn>>> = OnceLock::new();

/// Registers a callback to run each time a bank becomes optimistically confirmed.
pub fn register_confirmed_verifier(verifier: ConfirmedVerifierFn) {
    CONFIRMED_VERIFIERS
        .get_or_init(|| RwLock::new(Vec::new()))
        .write()
        .unwrap()
        .push(verifier);
}

/// Runs all registered confirmed verifiers against `bank`.
///
/// `bank` must be frozen and optimistically confirmed; callers must invoke this
/// in ascending slot order.
pub fn run_confirmed_verifiers(bank: &Arc<Bank>) {
    let Some(verifiers) = CONFIRMED_VERIFIERS.get() else {
        return;
    };
    for verifier in verifiers.read().unwrap().iter() {
        verifier(bank);
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::bank::Bank,
        solana_genesis_config::create_genesis_config,
        std::sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    };

    static CONFIRMED_SLOT_SUM: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn run_confirmed_verifiers_invokes_registered_callbacks() {
        CONFIRMED_SLOT_SUM.store(0, Ordering::Relaxed);
        super::register_confirmed_verifier(Box::new(|bank| {
            CONFIRMED_SLOT_SUM.fetch_add(bank.slot() + 1, Ordering::Relaxed);
        }));

        let (genesis_config, _mint_keypair) = create_genesis_config(500);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        bank.freeze();
        super::run_confirmed_verifiers(&bank);

        assert_eq!(CONFIRMED_SLOT_SUM.load(Ordering::Relaxed), 1);
    }
}
