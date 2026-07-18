use {
    log::{info, warn},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::{
        accounts_index::{AccountIndex, IndexKey},
        accounts_scan::ScanResult,
        is_loadable::IsLoadable as _,
    },
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, confirmed_verifier::register_confirmed_verifier},
    std::{
        collections::HashSet,
        sync::{Arc, Once, OnceLock, RwLock},
        time::Instant,
    },
};

pub mod account_indexes;
pub(crate) mod chain_deltas;
pub mod core_yt_balance;
pub mod token_total_supply;
pub use account_indexes::{
    account_secondary_indexes_for_registered_verifiers, merge_account_secondary_indexes,
    registered_program_ids,
};
pub use core_yt_balance::CoreYtBalanceVerifier;
pub use token_total_supply::TokenTotalSupplyVerifier;

/// Log progress every N accounts when scanning the SPL Token program index.
const TOKEN_PROGRAM_PROGRESS_INTERVAL: usize = 1_000_000;
/// Log progress every N accounts for other program indexes.
const DEFAULT_PROGRAM_PROGRESS_INTERVAL: usize = 100_000;

/// verifies state for accounts owned by a fixed set of programs.
///
/// All verifiers run at the confirmed tier: they are invoked once per
/// optimistically confirmed bank, in ascending slot order, on the
/// `OptimisticallyConfirmedBankTracker` thread. Confirmed banks are frozen,
/// will not revert, and still have live parent chains (roots trail
/// confirmation), so incremental verifiers can walk `Bank::parent()` for
/// per-slot deltas without being fork-aware.
pub trait BlockVerify: Send {
    /// program IDs whose owned accounts should be scanned.
    const PROGRAM_IDS: &'static [Pubkey];

    /// called once before scanning accounts at the confirmed bank.
    fn new(bank: &Bank) -> Self;

    /// called for each loadable account owned by one of [`Self::PROGRAM_IDS`].
    fn on_account(&mut self, pubkey: &Pubkey, account: &AccountSharedData);

    /// called after all matching accounts have been scanned.
    fn after_scan(&self, bank: &Bank);

    /// If true, this verifier handled the bank itself (e.g. incremental cache)
    /// and should be skipped by the shared account scan.
    fn try_self_verify(_bank: &Arc<Bank>) -> bool {
        false
    }
}

trait ErasedBlockVerify: Send {
    fn on_account(&mut self, pubkey: &Pubkey, account: &AccountSharedData);
    fn after_scan(&self, bank: &Bank);
}

impl<V: BlockVerify> ErasedBlockVerify for V {
    fn on_account(&mut self, pubkey: &Pubkey, account: &AccountSharedData) {
        BlockVerify::on_account(self, pubkey, account);
    }

    fn after_scan(&self, bank: &Bank) {
        BlockVerify::after_scan(self, bank);
    }
}

struct VerifierEntry {
    program_ids: HashSet<Pubkey>,
    init: fn(&Bank) -> Box<dyn ErasedBlockVerify>,
    self_verify: fn(&Arc<Bank>) -> bool,
}

static VERIFIERS: OnceLock<RwLock<Vec<VerifierEntry>>> = OnceLock::new();
static CONFIRMED_HOOK_INSTALLED: Once = Once::new();

fn verifier_entries() -> &'static RwLock<Vec<VerifierEntry>> {
    VERIFIERS.get_or_init(|| RwLock::new(Vec::new()))
}

/// All registered verifier entries, for secondary-index config.
pub(crate) fn verifiers() -> Vec<VerifierEntrySnapshot> {
    let Some(lock) = VERIFIERS.get() else {
        return Vec::new();
    };
    lock.read()
        .unwrap()
        .iter()
        .map(VerifierEntrySnapshot::from)
        .collect()
}

pub(crate) struct VerifierEntrySnapshot {
    pub program_ids: HashSet<Pubkey>,
}

impl From<&VerifierEntry> for VerifierEntrySnapshot {
    fn from(entry: &VerifierEntry) -> Self {
        Self {
            program_ids: entry.program_ids.clone(),
        }
    }
}

/// registers `V` to run on every optimistically confirmed bank.
///
/// Verifiers that do not self-verify share one account scan per confirmed slot.
pub fn register_block_verifier<V: BlockVerify + Send + 'static>() {
    fn init<V: BlockVerify + Send + 'static>(bank: &Bank) -> Box<dyn ErasedBlockVerify> {
        Box::new(V::new(bank))
    }

    let entry = VerifierEntry {
        program_ids: V::PROGRAM_IDS.iter().copied().collect(),
        init: init::<V>,
        self_verify: V::try_self_verify,
    };

    verifier_entries().write().unwrap().push(entry);
    CONFIRMED_HOOK_INSTALLED.call_once(|| {
        register_confirmed_verifier(Box::new(|bank| run_registered_block_verifiers(bank)));
    });
}

/// runs every registered [`BlockVerify`] against the confirmed `bank`.
///
/// Self-verifying verifiers handle the bank incrementally; the rest share a
/// single account scan.
pub fn run_registered_block_verifiers(bank: &Arc<Bank>) {
    run_verifiers_internal(bank, false);
}

/// runs every registered [`BlockVerify`] against `bank` using the shared
/// account scan, bypassing incremental self-verification.
///
/// Intended for one-shot offline runs (e.g. ledger-tool against a snapshot
/// bank) where scheduling a background rebuild would never complete.
pub fn run_registered_block_verifiers_blocking(bank: &Arc<Bank>) {
    run_verifiers_internal(bank, true);
}

fn run_verifiers_internal(bank: &Arc<Bank>, force_shared_scan: bool) {
    let entries = verifier_entries().read().unwrap();
    if entries.is_empty() {
        return;
    }

    let mut active: Vec<(HashSet<Pubkey>, Box<dyn ErasedBlockVerify>)> = Vec::new();
    for entry in entries.iter() {
        if !force_shared_scan && (entry.self_verify)(bank) {
            continue;
        }
        active.push((entry.program_ids.clone(), (entry.init)(bank)));
    }

    if active.is_empty() {
        return;
    }

    let target_program_ids: HashSet<Pubkey> = active
        .iter()
        .flat_map(|(program_ids, _)| program_ids.iter().copied())
        .collect();

    let started = Instant::now();
    let use_program_index = program_id_index_enabled(bank, &target_program_ids);
    info!(
        "block verifier: starting slot={} programs={} use_program_index={use_program_index}",
        bank.slot(),
        target_program_ids.len(),
    );

    let scan_result = if use_program_index {
        scan_registered_verifiers_by_program_index(bank, &target_program_ids, &mut active)
    } else {
        scan_registered_verifiers_full(bank, &target_program_ids, &mut active)
    };

    if let Err(err) = scan_result {
        warn!(
            "block verifier scan failed for slot {}: {err}",
            bank.slot(),
        );
        return;
    }

    for (_, verifier) in &active {
        verifier.after_scan(bank);
    }

    info!(
        "block verifier: finished slot={} elapsed_us={}",
        bank.slot(),
        started.elapsed().as_micros(),
    );
}

fn program_id_index_enabled(bank: &Bank, target_program_ids: &HashSet<Pubkey>) -> bool {
    bank.rc
        .accounts
        .accounts_db
        .account_indexes
        .contains(&AccountIndex::ProgramId)
        && target_program_ids
            .iter()
            .all(|program_id| bank.account_indexes_include_key(program_id))
}

fn progress_interval_for_program(program_id: &Pubkey) -> usize {
    if program_id == &spl_token_interface::ID {
        TOKEN_PROGRAM_PROGRESS_INTERVAL
    } else {
        DEFAULT_PROGRAM_PROGRESS_INTERVAL
    }
}

fn scan_registered_verifiers_full(
    bank: &Bank,
    target_program_ids: &HashSet<Pubkey>,
    active: &mut [(HashSet<Pubkey>, Box<dyn ErasedBlockVerify>)],
) -> ScanResult<()> {
    let mut fetched = 0usize;
    bank.scan_all_accounts(|account_tuple| {
        let Some((pubkey, account, _slot)) = account_tuple else {
            return;
        };
        fetched += 1;
        if fetched % DEFAULT_PROGRAM_PROGRESS_INTERVAL == 0 {
            info!(
                "block verifier: slot={} full_scan fetched={fetched}",
                bank.slot(),
            );
        }
        dispatch_account_to_verifiers(pubkey, account, target_program_ids, active);
    })?;
    info!(
        "block verifier: slot={} full_scan fetched={fetched}",
        bank.slot(),
    );
    Ok(())
}

fn scan_registered_verifiers_by_program_index(
    bank: &Bank,
    target_program_ids: &HashSet<Pubkey>,
    active: &mut [(HashSet<Pubkey>, Box<dyn ErasedBlockVerify>)],
) -> ScanResult<()> {
    for program_id in target_program_ids {
        let progress_interval = progress_interval_for_program(program_id);
        let mut fetched = 0usize;
        let program_started = Instant::now();

        info!(
            "block verifier: slot={} scanning program={program_id} (progress every {progress_interval})",
            bank.slot(),
        );

        bank.scan_filtered_indexed_accounts(
            &IndexKey::ProgramId(*program_id),
            |account| account.is_loadable() && account.owner() == program_id,
            |pubkey, account| {
                fetched += 1;
                if fetched % progress_interval == 0 {
                    info!(
                        "block verifier: slot={} program={program_id} fetched={fetched}",
                        bank.slot(),
                    );
                }
                dispatch_account_to_verifiers(pubkey, account, target_program_ids, active);
            },
        )?;

        info!(
            "block verifier: slot={} program={program_id} fetched={fetched} scan_us={}",
            bank.slot(),
            program_started.elapsed().as_micros(),
        );
    }
    Ok(())
}

fn dispatch_account_to_verifiers(
    pubkey: &Pubkey,
    account: AccountSharedData,
    target_program_ids: &HashSet<Pubkey>,
    active: &mut [(HashSet<Pubkey>, Box<dyn ErasedBlockVerify>)],
) {
    if !account.is_loadable() {
        return;
    }
    if !target_program_ids.contains(account.owner()) {
        return;
    }
    for (program_ids, verifier) in active.iter_mut() {
        if program_ids.contains(account.owner()) {
            verifier.on_account(pubkey, &account);
        }
    }
}

/// scans accounts owned by [`BlockVerify::PROGRAM_IDS`] and drives `V`'s lifecycle hooks.
///
/// prefer registering verifiers with [`register_block_verifier`] so multiple
/// verifiers can share one scan per slot.
pub fn run_block_verifier<V: BlockVerify + Send>(bank: &Bank) -> ScanResult<V> {
    let program_ids: HashSet<Pubkey> = V::PROGRAM_IDS.iter().copied().collect();
    let mut verifier = V::new(bank);

    bank.scan_all_accounts(|account_tuple| {
        let Some((pubkey, account, _slot)) = account_tuple else {
            return;
        };
        if !account.is_loadable() {
            return;
        }
        if program_ids.contains(account.owner()) {
            verifier.on_account(pubkey, &account);
        }
    })?;

    verifier.after_scan(bank);
    Ok(verifier)
}

static DEFAULT_VERIFIERS_INSTALLED: Once = Once::new();

/// registers built-in block verifiers.
pub fn install_default_block_verifiers() {
    DEFAULT_VERIFIERS_INSTALLED.call_once(|| {
        register_block_verifier::<CoreYtBalanceVerifier>();
        register_block_verifier::<TokenTotalSupplyVerifier>();
    });
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_genesis_config::create_genesis_config,
        solana_runtime::bank::Bank,
        std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
    };

    static TEST_PROGRAM_ID: Pubkey = Pubkey::new_from_array([7; 32]);
    static OTHER_PROGRAM_ID: Pubkey = Pubkey::new_from_array([8; 32]);
    static SEEN_ACCOUNTS: Mutex<Vec<Pubkey>> = Mutex::new(Vec::new());
    static DISPATCH_AFTER_SCAN_COUNT: AtomicUsize = AtomicUsize::new(0);
    static A_SEEN: Mutex<Vec<Pubkey>> = Mutex::new(Vec::new());
    static B_SEEN: Mutex<Vec<Pubkey>> = Mutex::new(Vec::new());

    struct TestVerifier;

    impl BlockVerify for TestVerifier {
        const PROGRAM_IDS: &'static [Pubkey] = &[TEST_PROGRAM_ID];

        fn new(_bank: &Bank) -> Self {
            Self
        }

        fn on_account(&mut self, pubkey: &Pubkey, _account: &AccountSharedData) {
            SEEN_ACCOUNTS.lock().unwrap().push(*pubkey);
        }

        fn after_scan(&self, _bank: &Bank) {}
    }

    #[test]
    fn run_block_verifier_finds_program_accounts() {
        SEEN_ACCOUNTS.lock().unwrap().clear();

        let (genesis_config, _mint_keypair) = create_genesis_config(500);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        let pubkey = Pubkey::new_unique();
        let account = AccountSharedData::new(1, 8, &TEST_PROGRAM_ID);
        bank.store_account(&pubkey, &account);

        run_block_verifier::<TestVerifier>(&bank).unwrap();

        assert_eq!(*SEEN_ACCOUNTS.lock().unwrap(), vec![pubkey]);
    }

    struct DispatchVerifier;

    impl BlockVerify for DispatchVerifier {
        const PROGRAM_IDS: &'static [Pubkey] = &[TEST_PROGRAM_ID];

        fn new(_bank: &Bank) -> Self {
            Self
        }

        fn on_account(&mut self, _pubkey: &Pubkey, _account: &AccountSharedData) {}

        fn after_scan(&self, _bank: &Bank) {
            DISPATCH_AFTER_SCAN_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct ProgramAVerifier;

    impl BlockVerify for ProgramAVerifier {
        const PROGRAM_IDS: &'static [Pubkey] = &[TEST_PROGRAM_ID];

        fn new(_bank: &Bank) -> Self {
            Self
        }

        fn on_account(&mut self, pubkey: &Pubkey, _account: &AccountSharedData) {
            A_SEEN.lock().unwrap().push(*pubkey);
        }

        fn after_scan(&self, _bank: &Bank) {
            DISPATCH_AFTER_SCAN_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct ProgramBVerifier;

    impl BlockVerify for ProgramBVerifier {
        const PROGRAM_IDS: &'static [Pubkey] = &[OTHER_PROGRAM_ID];

        fn new(_bank: &Bank) -> Self {
            Self
        }

        fn on_account(&mut self, pubkey: &Pubkey, _account: &AccountSharedData) {
            B_SEEN.lock().unwrap().push(*pubkey);
        }

        fn after_scan(&self, _bank: &Bank) {
            DISPATCH_AFTER_SCAN_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn register_block_verifier_runs_on_dispatch() {
        DISPATCH_AFTER_SCAN_COUNT.store(0, Ordering::Relaxed);
        register_block_verifier::<DispatchVerifier>();

        let (genesis_config, _mint_keypair) = create_genesis_config(500);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        run_registered_block_verifiers(&bank);

        assert!(DISPATCH_AFTER_SCAN_COUNT.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn multiple_verifiers_share_one_scan() {
        A_SEEN.lock().unwrap().clear();
        B_SEEN.lock().unwrap().clear();

        register_block_verifier::<ProgramAVerifier>();
        register_block_verifier::<ProgramBVerifier>();

        let (genesis_config, _mint_keypair) = create_genesis_config(500);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        let test_pubkey = Pubkey::new_unique();
        bank.store_account(
            &test_pubkey,
            &AccountSharedData::new(1, 8, &TEST_PROGRAM_ID),
        );
        let other_pubkey = Pubkey::new_unique();
        bank.store_account(
            &other_pubkey,
            &AccountSharedData::new(1, 8, &OTHER_PROGRAM_ID),
        );

        run_registered_block_verifiers(&bank);

        assert_eq!(*A_SEEN.lock().unwrap(), vec![test_pubkey]);
        assert_eq!(*B_SEEN.lock().unwrap(), vec![other_pubkey]);
    }
}
