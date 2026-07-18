use {
    crate::{
        BlockVerify,
        chain_deltas::{AccountDelta, PendingSlotDelta, WarmupWindow, buffer_chain_deltas,
            collect_parent_chain_deltas},
    },
    log::{error, info, warn},
    scc::HashMap as SccHashMap,
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::{
        accounts::Accounts, accounts_index::IndexKey, ancestors::Ancestors,
        is_loadable::IsLoadable as _,
    },
    solana_clock::{BankId, Slot},
    solana_program_pack::Pack,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    spl_token_interface::state::{Account as SplTokenAccount, AccountState, Mint},
    std::{
        collections::{HashMap, HashSet, VecDeque},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
        thread,
        time::Instant,
    },
};

const FULL_SCAN_PROGRESS_INTERVAL: usize = 1_000_000;
/// Log catch-up progress every N confirmed ancestor slots processed.
const CATCH_UP_PROGRESS_INTERVAL: Slot = 1_000;
/// Max per-slot account-update batches buffered while a background rebuild runs.
const PENDING_SLOT_CAP: usize = 8192;

#[derive(Debug, Clone, Copy)]
struct TokenAccountLite {
    mint: Pubkey,
    amount: u64,
}

#[derive(Default)]
struct TokenSupplyState {
    slot: Slot,
    /// Concurrent maps so rebuild can upsert in parallel with `do_load`.
    mint_supplies: SccHashMap<Pubkey, u64>,
    /// Per-mint sum of tracked token account amounts, maintained with
    /// wrapping (mod 2^64) arithmetic.
    ///
    /// Incremental applies visit a slot's writes in arbitrary order, so a
    /// credit can land before its matching debit and transiently push the sum
    /// past `u64::MAX` for mints whose supply sits at the top of the range.
    /// Wrapping arithmetic is commutative and associative, so ordering cannot
    /// lose information: the wrapped excess is exactly undone when the debit
    /// lands. (Saturating math here silently and permanently lost the excess —
    /// the 2026-07 mainnet false-undercount incident.)
    ///
    /// Mod-2^64 caveats, accepted for the 8-byte/mint savings over `u128`: a
    /// violation where `Σ − supply` is an exact multiple of 2^64 is invisible,
    /// and a logged `token_account_sum` for a genuinely-over-2^64 violation is
    /// the wrapped value, not the true sum.
    token_amounts_by_mint: SccHashMap<Pubkey, u64>,
    token_accounts: SccHashMap<Pubkey, TokenAccountLite>,
    /// Mints whose mint.supply != sum(token amounts). Maintained incrementally after
    /// the initial full seed so per-slot checks only touch updated mints.
    mismatched_mints: HashSet<Pubkey>,
    mints_seen: AtomicUsize,
    token_accounts_seen: AtomicUsize,
}

impl TokenSupplyState {
    fn mints_seen(&self) -> usize {
        self.mints_seen.load(Ordering::Relaxed)
    }

    fn token_accounts_seen(&self) -> usize {
        self.token_accounts_seen.load(Ordering::Relaxed)
    }

    fn clear_account(&self, pubkey: &Pubkey, mut dirty: Option<&mut HashSet<Pubkey>>) {
        if self.mint_supplies.remove_sync(pubkey).is_some() {
            self.mints_seen.fetch_sub(1, Ordering::Relaxed);
            if let Some(dirty) = dirty.as_mut() {
                dirty.insert(*pubkey);
            }
        }
        if let Some((_, token)) = self.token_accounts.remove_sync(pubkey) {
            self.token_accounts_seen.fetch_sub(1, Ordering::Relaxed);
            if let Some(mut occupied) = self.token_amounts_by_mint.get_sync(&token.mint) {
                let total = occupied.get_mut();
                *total = total.wrapping_sub(token.amount);
                // A missing entry reads as 0, so dropping a (possibly wrapped)
                // zero preserves the mod-2^64 sum exactly.
                if *total == 0 {
                    let _ = occupied.remove();
                }
            }
            if let Some(dirty) = dirty.as_mut() {
                dirty.insert(token.mint);
            }
        }
    }

    /// Safe for concurrent callers: each map operation releases before the next.
    fn upsert_account(
        &self,
        pubkey: &Pubkey,
        account: &AccountSharedData,
        mut dirty: Option<&mut HashSet<Pubkey>>,
    ) {
        self.clear_account(pubkey, dirty.as_deref_mut());

        if !account.is_loadable() || account.owner() != &spl_token_interface::ID {
            return;
        }

        let data = account.data();
        if data.len() == Mint::LEN {
            if let Ok(mint) = Mint::unpack(data) {
                if mint.is_initialized {
                    let mints_seen = self.mints_seen.fetch_add(1, Ordering::Relaxed) + 1;
                    if mints_seen % FULL_SCAN_PROGRESS_INTERVAL == 0 {
                        info!(
                            "token total supply verifier: mints_fetched={} token_accounts={}",
                            mints_seen,
                            self.token_accounts_seen(),
                        );
                    }
                    self.mint_supplies.upsert_sync(*pubkey, mint.supply);
                    if let Some(dirty) = dirty.as_mut() {
                        dirty.insert(*pubkey);
                    }
                }
            }
            return;
        }

        if data.len() == SplTokenAccount::LEN {
            if let Ok(token) = SplTokenAccount::unpack(data) {
                if token.state != AccountState::Uninitialized {
                    self.token_accounts_seen.fetch_add(1, Ordering::Relaxed);
                    self.token_amounts_by_mint
                        .entry_sync(token.mint)
                        .and_modify(|total| *total = total.wrapping_add(token.amount))
                        .or_insert(token.amount);
                    self.token_accounts.upsert_sync(
                        *pubkey,
                        TokenAccountLite {
                            mint: token.mint,
                            amount: token.amount,
                        },
                    );
                    if let Some(dirty) = dirty.as_mut() {
                        dirty.insert(token.mint);
                    }
                }
            }
        }
    }

    fn tracks(&self, pubkey: &Pubkey) -> bool {
        self.mint_supplies.contains_sync(pubkey) || self.token_accounts.contains_sync(pubkey)
    }

    /// One-time full scan used after rebuild to seed [`Self::mismatched_mints`].
    fn seed_mismatched_mints(&mut self) {
        self.mismatched_mints.clear();
        let mint_supplies = &self.mint_supplies;
        let token_amounts_by_mint = &self.token_amounts_by_mint;
        let mismatched_mints = &mut self.mismatched_mints;
        mint_supplies.iter_sync(|mint, mint_supply| {
            let token_account_sum = token_amounts_by_mint
                .read_sync(mint, |_, sum| *sum)
                .unwrap_or(0);
            if *mint_supply != token_account_sum {
                mismatched_mints.insert(*mint);
                warn!(
                    "token supply mismatch mint={mint} mint_supply={mint_supply} \
                     token_account_sum={token_account_sum}"
                );
            }
            true
        });
    }

    /// Recheck only `dirty` mints; migrate them into/out of [`Self::mismatched_mints`].
    /// Untouched mismatched mints are left as-is.
    fn recheck_dirty_mints(&mut self, dirty: HashSet<Pubkey>) {
        for mint in dirty {
            let Some(mint_supply) = self.mint_supplies.read_sync(&mint, |_, supply| *supply) else {
                self.mismatched_mints.remove(&mint);
                continue;
            };
            let token_account_sum = self
                .token_amounts_by_mint
                .read_sync(&mint, |_, sum| *sum)
                .unwrap_or(0);
            if mint_supply == token_account_sum {
                self.mismatched_mints.remove(&mint);
            } else {
                self.mismatched_mints.insert(mint);
            }
        }
    }

    fn mismatch_counts(&self) -> (usize, usize) {
        let num_mismatch = self.mismatched_mints.len();
        let num_match = self.mints_seen().saturating_sub(num_mismatch);
        (num_match, num_mismatch)
    }

    /// Log every mint currently in [`Self::mismatched_mints`] with live supply totals.
    fn log_all_mismatches(&self) {
        let mut mints: Vec<_> = self.mismatched_mints.iter().copied().collect();
        mints.sort_unstable();
        for mint in mints {
            let mint_supply = self
                .mint_supplies
                .read_sync(&mint, |_, supply| *supply)
                .unwrap_or(0);
            let token_account_sum = self
                .token_amounts_by_mint
                .read_sync(&mint, |_, sum| *sum)
                .unwrap_or(0);
            warn!(
                "token supply mismatch mint={mint} mint_supply={mint_supply} \
                 token_account_sum={token_account_sum}"
            );
        }
    }
}

/// Everything the background rebuild needs from the scheduling confirmed bank.
///
/// The scan loads with the bank's own `ancestors`, so the snapshot is exactly the
/// bank's fork view at `slot` even while newer slots root mid-scan. Buffered slot
/// deltas strictly after `slot` are applied on top when the scan finishes.
#[derive(Clone)]
struct ScanContext {
    accounts: Arc<Accounts>,
    ancestors: Ancestors,
    bank_id: BankId,
    slot: Slot,
}

impl ScanContext {
    fn from_bank(bank: &Bank) -> Self {
        Self {
            accounts: Arc::clone(&bank.rc.accounts),
            ancestors: bank.ancestors.clone(),
            bank_id: bank.bank_id(),
            slot: bank.slot(),
        }
    }
}

enum CacheState {
    Cold,
    /// Waiting for confirmed-slot progression to steady before scheduling a
    /// full rebuild; a rebuild scheduled while the node is replaying backlog
    /// would have its pending buffer overtaken by roots and be discarded.
    Warming(WarmupWindow),
    Building {
        base_slot: Slot,
        /// Highest confirmed slot whose deltas have been buffered (or drained
        /// by the rebuild thread). Buffering resumes from here, NOT from
        /// `pending.back()`: the rebuild thread drains `pending` in chunks
        /// while the scan's backlog is applied, and re-anchoring at a drained
        /// (long-rooted) slot would sever the parent-chain walk.
        buffered_through: Slot,
        /// Per-slot account writes captured while the full scan runs; applied
        /// after the scan (all are `> base_slot` by construction).
        pending: VecDeque<PendingSlotDelta>,
        invalidated: bool,
    },
    Ready(TokenSupplyState),
}

static STATE_CACHE: Mutex<CacheState> = Mutex::new(CacheState::Cold);

/// Confirmed-tier verify: background full rebuild on first confirmed bank, then
/// per-slot catch-up along the confirmed chain.
///
/// Called once per optimistically confirmed bank, in ascending slot order, on the
/// tracker thread (never concurrently). Confirmed banks do not revert and their
/// parent chains are live, so no fork handling is needed.
pub fn verify_token_total_supply(bank: &Arc<Bank>) {
    let mut cache = STATE_CACHE.lock().unwrap();
    match &mut *cache {
        CacheState::Building {
            base_slot,
            buffered_through,
            pending,
            invalidated,
        } => {
            buffer_chain_deltas(
                "token total supply verifier",
                bank,
                *buffered_through,
                pending,
                invalidated,
                PENDING_SLOT_CAP,
            );
            if !*invalidated {
                *buffered_through = bank.slot();
            }
            if bank.slot() % 1000 == 0 {
                info!(
                    "token total supply verifier: rebuild in progress base_slot={base_slot} \
                     buffered_through={buffered_through} buffered_slots={} tip_slot={} \
                     invalidated={invalidated}",
                    pending.len(),
                    bank.slot(),
                );
            }
        }
        CacheState::Ready(state) if state.slot >= bank.slot() => {
            // Confirmed notifications arrive in ascending order; this is defensive.
            info!(
                "token total supply verifier: slot={} mode=already_confirmed cache_slot={}",
                bank.slot(),
                state.slot,
            );
        }
        CacheState::Ready(_) => {
            // Take the state out (the placeholder is never observed: the lock is
            // held for the whole catch-up and hook calls are serial).
            let CacheState::Ready(mut state) =
                std::mem::replace(&mut *cache, CacheState::Cold)
            else {
                unreachable!();
            };
            match catch_up_to_confirmed(&mut state, bank) {
                Ok((_modified, dirty)) => {
                    let dirty_rechecked = dirty.len();
                    state.recheck_dirty_mints(dirty);
                    let (num_match, num_mismatch) = state.mismatch_counts();
                    info!(
                        "token total supply verifier: slot={} mode=catch_up mints={} \
                         token_accounts={} num_match={num_match} num_mismatch={num_mismatch} \
                         dirty_rechecked={dirty_rechecked}",
                        state.slot,
                        state.mints_seen(),
                        state.token_accounts_seen(),
                    );
                    if num_mismatch > 0 {
                        state.log_all_mismatches();
                        error!("token total supply invariant failed for {num_mismatch} mint(s)");
                    }
                    *cache = CacheState::Ready(state);
                }
                Err(reason) => {
                    info!(
                        "token total supply verifier: catch_up unavailable at slot={} \
                         ({reason}); deferring full rebuild until confirmed-slot \
                         progression is steady",
                        bank.slot(),
                    );
                    *cache = CacheState::Warming(WarmupWindow::new(bank.slot()));
                }
            }
        }
        CacheState::Warming(window) => {
            if window.observe("token total supply verifier", bank.slot()) {
                start_background_rebuild(ScanContext::from_bank(bank), &mut cache);
            }
        }
        CacheState::Cold => {
            *cache = CacheState::Warming(WarmupWindow::new(bank.slot()));
        }
    }
}

/// Apply live parent-chain deltas from `state.slot` to the confirmed `bank`.
fn catch_up_to_confirmed(
    state: &mut TokenSupplyState,
    bank: &Bank,
) -> Result<(usize, HashSet<Pubkey>), &'static str> {
    let from_slot = state.slot;
    let to_slot = bank.slot();
    let started = Instant::now();
    let deltas = collect_parent_chain_deltas(from_slot, bank)?;

    let mut dirty = HashSet::new();
    let mut modified = 0usize;
    let mut processed = 0usize;
    let total = deltas.len();
    for (slot, slot_deltas) in deltas {
        modified += apply_delta_slice(state, &slot_deltas, &mut dirty);
        state.slot = slot;
        processed += 1;
        if processed as Slot % CATCH_UP_PROGRESS_INTERVAL == 0 {
            info!(
                "token total supply verifier: catch_up progress at_slot={slot} \
                 processed_slots={processed}/{total} modified={modified} dirty_mints={}",
                dirty.len(),
            );
        }
    }
    state.slot = to_slot;

    if processed > 1 {
        info!(
            "token total supply verifier: catch_up from_slot={from_slot} to_slot={to_slot} \
             processed_slots={processed} modified={modified} dirty_mints={} elapsed_us={}",
            dirty.len(),
            started.elapsed().as_micros()
        );
    }
    Ok((modified, dirty))
}

fn start_background_rebuild(scan_ctx: ScanContext, cache: &mut CacheState) {
    let base_slot = scan_ctx.slot;
    *cache = CacheState::Building {
        base_slot,
        buffered_through: base_slot,
        pending: VecDeque::new(),
        invalidated: false,
    };

    info!(
        "token total supply verifier: scheduling background full rebuild slot={base_slot} \
         (confirmed tier; buffering up to {PENDING_SLOT_CAP} slot updates)"
    );

    let spawn_result = thread::Builder::new()
        .name("token-supply-rebuild".into())
        .spawn(move || finish_background_rebuild(scan_ctx));

    if let Err(err) = spawn_result {
        warn!("token total supply verifier: failed to spawn rebuild thread: {err}");
        *cache = CacheState::Cold;
    }
}

fn finish_background_rebuild(scan_ctx: ScanContext) {
    let base_slot = scan_ctx.slot;
    let result = rebuild_state(
        &scan_ctx.accounts,
        &scan_ctx.ancestors,
        scan_ctx.bank_id,
        base_slot,
    );

    let mut state = match result {
        Ok(state) => state,
        Err(err) => {
            warn!(
                "token total supply verifier: background full scan failed for slot {base_slot}: {err}"
            );
            let mut cache = STATE_CACHE.lock().unwrap();
            if let CacheState::Building {
                base_slot: building_slot,
                ..
            } = &*cache
            {
                if *building_slot == base_slot {
                    *cache = CacheState::Cold;
                }
            }
            return;
        }
    };

    // Seed mismatches from the rebuild snapshot before pending updates mutate
    // sums. Runs WITHOUT the cache lock (`state` is exclusively owned here):
    // iterating all mints takes tens of seconds and the confirmed hook must
    // keep buffering meanwhile.
    state.seed_mismatched_mints();
    let seed_mismatch = state.mismatched_mints.len();
    let scan_slot = state.slot;

    let mut dirty = HashSet::new();
    let mut modified = 0usize;
    let mut buffered_slots = 0usize;
    let mut applied_slots = 0usize;
    let mut skipped_slots = 0usize;

    // Drain-and-apply loop: hold the cache lock only to take buffered slots
    // out (or to install the finished state); apply them unlocked. Applying
    // the multi-minute backlog while holding the lock stalls the confirmed
    // hook, roots overtake the buffer anchor meanwhile, and the first
    // catch-up after Ready finds severed parent chains and forces another
    // full rebuild (observed on mainnet: a 22s seed+apply hold looped the
    // verifier through back-to-back rebuilds).
    loop {
        let batch = {
            let mut cache = STATE_CACHE.lock().unwrap();
            let CacheState::Building {
                base_slot: building_slot,
                pending,
                invalidated,
                ..
            } = &mut *cache
            else {
                return;
            };
            if *building_slot != base_slot {
                return;
            }
            if *invalidated {
                warn!(
                    "token total supply verifier: discarding rebuild for slot={base_slot} after \
                     pending gap/overflow"
                );
                *cache = CacheState::Cold;
                return;
            }
            if pending.is_empty() {
                // Everything buffered so far is applied. Install under this
                // same lock so no slot can slip between drain and install.
                let dirty_rechecked = dirty.len();
                state.recheck_dirty_mints(std::mem::take(&mut dirty));
                let (num_match, num_mismatch) = state.mismatch_counts();
                info!(
                    "token total supply verifier: slot={} mode=full_background mints={} \
                     token_accounts={} num_match={num_match} num_mismatch={num_mismatch} \
                     seed_mismatch={seed_mismatch} dirty_rechecked={dirty_rechecked} \
                     scan_slot={scan_slot} buffered_slots={buffered_slots} \
                     applied_slots={applied_slots} skipped_already_scanned={skipped_slots} \
                     pending_modified={modified}",
                    state.slot,
                    state.mints_seen(),
                    state.token_accounts_seen(),
                );
                if num_mismatch > 0 {
                    state.log_all_mismatches();
                    error!("token total supply invariant failed for {num_mismatch} mint(s)");
                }
                *cache = CacheState::Ready(state);
                return;
            }
            std::mem::take(pending)
        };

        for (slot, deltas) in batch {
            buffered_slots += 1;
            // Buffering starts strictly after base_slot (the scan snapshot
            // slot), so this should never skip anything; kept as a guard.
            if slot <= scan_slot {
                skipped_slots += 1;
                continue;
            }
            modified += apply_delta_slice(&mut state, &deltas, &mut dirty);
            state.slot = slot;
            applied_slots += 1;
        }
    }
}

fn apply_delta_slice(
    state: &mut TokenSupplyState,
    deltas: &[AccountDelta],
    dirty: &mut HashSet<Pubkey>,
) -> usize {
    let mut touched = 0usize;
    for (pubkey, account) in deltas {
        let owned = account.is_loadable() && account.owner() == &spl_token_interface::ID;
        if owned || state.tracks(pubkey) {
            state.upsert_account(pubkey, account, Some(dirty));
            touched += 1;
        }
    }
    touched
}

fn rebuild_state(
    accounts: &Accounts,
    ancestors: &Ancestors,
    bank_id: BankId,
    slot: Slot,
) -> Result<TokenSupplyState, String> {
    // The parallel scan loads with the confirmed bank's own ancestors, so the
    // result is a consistent snapshot at `slot` (the bank's fork view) even
    // while newer slots keep rooting mid-scan.
    let mut state = TokenSupplyState::default();
    let fetched = AtomicUsize::new(0);
    let started = Instant::now();

    info!("token total supply verifier: full rebuild starting slot={slot}");

    accounts
        .scan_by_index_key_parallel(
            ancestors,
            bank_id,
            &IndexKey::ProgramId(spl_token_interface::ID),
            |account| account.is_loadable() && account.owner() == &spl_token_interface::ID,
            |pubkey, account| {
                let n = fetched.fetch_add(1, Ordering::Relaxed) + 1;
                if n % FULL_SCAN_PROGRESS_INTERVAL == 0 {
                    info!(
                        "token total supply verifier: full rebuild slot={slot} fetched={n} \
                         mints={} token_accounts={}",
                        state.mints_seen(),
                        state.token_accounts_seen(),
                    );
                }
                state.upsert_account(pubkey, &account, None);
            },
        )
        .map_err(|err| err.to_string())?;

    // The program-id index can be configured off (e.g. tests); fall back to a
    // full sweep so the verifier still works, just slower.
    if fetched.load(Ordering::Relaxed) == 0 {
        accounts
            .scan_all(ancestors, bank_id, |account_tuple| {
                let Some((pubkey, account, _slot)) = account_tuple else {
                    return;
                };
                if !account.is_loadable() || account.owner() != &spl_token_interface::ID {
                    return;
                }
                let n = fetched.fetch_add(1, Ordering::Relaxed) + 1;
                if n % FULL_SCAN_PROGRESS_INTERVAL == 0 {
                    info!(
                        "token total supply verifier: full rebuild slot={slot} fetched={n} \
                         mints={} token_accounts={}",
                        state.mints_seen(),
                        state.token_accounts_seen(),
                    );
                }
                state.upsert_account(pubkey, &account, None);
            })
            .map_err(|err| err.to_string())?;
    }

    state.slot = slot;

    info!(
        "token total supply verifier: full rebuild done slot={slot} fetched={} mints={} \
         token_accounts={} scan_us={}",
        fetched.load(Ordering::Relaxed),
        state.mints_seen(),
        state.token_accounts_seen(),
        started.elapsed().as_micros()
    );
    Ok(state)
}

pub struct TokenTotalSupplyVerifier {
    pub(crate) mint_supplies: HashMap<Pubkey, u64>,
    pub(crate) token_amounts_by_mint: HashMap<Pubkey, u64>,
    accounts_seen: usize,
    mints_seen: usize,
    token_accounts_seen: usize,
}

impl TokenTotalSupplyVerifier {
    pub fn new() -> Self {
        Self {
            mint_supplies: HashMap::new(),
            token_amounts_by_mint: HashMap::new(),
            accounts_seen: 0,
            mints_seen: 0,
            token_accounts_seen: 0,
        }
    }

    pub fn process_account(&mut self, pubkey: &Pubkey, account: &AccountSharedData) {
        self.accounts_seen += 1;
        let data = account.data();
        if data.len() == Mint::LEN {
            if let Ok(mint) = Mint::unpack(data) {
                if mint.is_initialized {
                    self.mints_seen += 1;
                    if self.mints_seen % FULL_SCAN_PROGRESS_INTERVAL == 0 {
                        info!(
                            "token total supply verifier: mints_fetched={} token_accounts={} \
                             accounts_seen={}",
                            self.mints_seen, self.token_accounts_seen, self.accounts_seen,
                        );
                    }
                    self.mint_supplies.insert(*pubkey, mint.supply);
                }
            }
        } else if data.len() == SplTokenAccount::LEN {
            if let Ok(token) = SplTokenAccount::unpack(data) {
                if token.state != AccountState::Uninitialized {
                    self.token_accounts_seen += 1;
                    let total = self.token_amounts_by_mint.entry(token.mint).or_default();
                    *total = total.wrapping_add(token.amount);
                }
            }
        }
    }

    pub fn report(&self, bank: &Bank) {
        let mut num_match = 0usize;
        let mut num_mismatch = 0usize;

        for (mint, mint_supply) in &self.mint_supplies {
            let token_account_sum = self.token_amounts_by_mint.get(mint).copied().unwrap_or(0);
            if *mint_supply == token_account_sum {
                num_match += 1;
            } else {
                num_mismatch += 1;
                warn!(
                    "token supply mismatch mint={mint} mint_supply={mint_supply} \
                     token_account_sum={token_account_sum}"
                );
            }
        }

        info!(
            "token total supply verifier: slot={} mode=legacy accounts_seen={} mints={} \
             token_accounts={} num_match={num_match} num_mismatch={num_mismatch}",
            bank.slot(),
            self.accounts_seen,
            self.mints_seen,
            self.token_accounts_seen,
        );

        if num_mismatch > 0 {
            error!("token total supply invariant failed for {num_mismatch} mint(s)");
        }
    }
}

impl BlockVerify for TokenTotalSupplyVerifier {
    const PROGRAM_IDS: &'static [Pubkey] = &[spl_token_interface::ID];

    fn try_self_verify(bank: &Arc<Bank>) -> bool {
        verify_token_total_supply(bank);
        true
    }

    fn new(_bank: &Bank) -> Self {
        Self::new()
    }

    fn on_account(&mut self, pubkey: &Pubkey, account: &AccountSharedData) {
        self.process_account(pubkey, account);
    }

    fn after_scan(&self, bank: &Bank) {
        self.report(bank);
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_account::{Account, AccountSharedData},
        solana_genesis_config::create_genesis_config,
        solana_program_option::COption,
        solana_runtime::bank::Bank,
        std::sync::Arc,
    };

    fn pack_mint(supply: u64) -> AccountSharedData {
        let mut data = vec![0u8; Mint::LEN];
        Mint {
            mint_authority: COption::None,
            supply,
            decimals: 9,
            is_initialized: true,
            freeze_authority: COption::None,
        }
        .pack_into_slice(&mut data);
        AccountSharedData::from(Account {
            lamports: 1,
            data,
            owner: spl_token_interface::ID,
            ..Account::default()
        })
    }

    fn pack_token_account(mint: Pubkey, amount: u64) -> AccountSharedData {
        let mut data = vec![0u8; SplTokenAccount::LEN];
        SplTokenAccount {
            mint,
            owner: Pubkey::new_unique(),
            amount,
            delegate: COption::None,
            state: AccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 0,
            close_authority: COption::None,
        }
        .pack_into_slice(&mut data);
        AccountSharedData::from(Account {
            lamports: 1,
            data,
            owner: spl_token_interface::ID,
            ..Account::default()
        })
    }

    #[test]
    fn detects_matching_and_mismatching_mint_supply() {
        let (genesis_config, _mint_keypair) = create_genesis_config(500);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        let good_mint = Pubkey::new_unique();
        let bad_mint = Pubkey::new_unique();
        bank.store_account(&good_mint, &pack_mint(100));
        bank.store_account(&bad_mint, &pack_mint(50));

        let good_token = Pubkey::new_unique();
        bank.store_account(&good_token, &pack_token_account(good_mint, 100));
        let bad_token = Pubkey::new_unique();
        bank.store_account(&bad_token, &pack_token_account(bad_mint, 40));

        let verifier = crate::run_block_verifier::<TokenTotalSupplyVerifier>(&bank).unwrap();

        assert_eq!(verifier.mint_supplies.get(&good_mint), Some(&100));
        assert_eq!(verifier.token_amounts_by_mint.get(&good_mint), Some(&100));
        assert_eq!(verifier.mint_supplies.get(&bad_mint), Some(&50));
        assert_eq!(verifier.token_amounts_by_mint.get(&bad_mint), Some(&40));
    }

    #[test]
    fn upsert_and_clear_adjusts_mint_sums() {
        let state = TokenSupplyState::default();
        let mint = Pubkey::new_unique();
        let token = Pubkey::new_unique();

        state.upsert_account(&mint, &pack_mint(100), None);
        state.upsert_account(&token, &pack_token_account(mint, 100), None);
        assert_eq!(
            state.token_amounts_by_mint.read_sync(&mint, |_, v| *v),
            Some(100)
        );

        state.upsert_account(&token, &pack_token_account(mint, 40), None);
        assert_eq!(
            state.token_amounts_by_mint.read_sync(&mint, |_, v| *v),
            Some(40)
        );

        state.upsert_account(&token, &AccountSharedData::default(), None);
        assert!(!state.token_accounts.contains_sync(&token));
        assert!(!state.token_amounts_by_mint.contains_sync(&mint));
    }

    #[test]
    fn recheck_dirty_mints_migrates_mismatch_set() {
        let mut state = TokenSupplyState::default();
        let good = Pubkey::new_unique();
        let bad = Pubkey::new_unique();
        let good_token = Pubkey::new_unique();
        let bad_token = Pubkey::new_unique();

        state.upsert_account(&good, &pack_mint(100), None);
        state.upsert_account(&good_token, &pack_token_account(good, 100), None);
        state.upsert_account(&bad, &pack_mint(50), None);
        state.upsert_account(&bad_token, &pack_token_account(bad, 40), None);
        state.seed_mismatched_mints();
        assert!(state.mismatched_mints.contains(&bad));
        assert!(!state.mismatched_mints.contains(&good));

        let mut dirty = HashSet::new();
        state.upsert_account(&bad_token, &pack_token_account(bad, 50), Some(&mut dirty));
        assert_eq!(dirty, HashSet::from([bad]));
        state.recheck_dirty_mints(dirty);
        assert!(state.mismatched_mints.is_empty());

        // Untouched good mint is not rechecked; set stays empty.
        let (num_match, num_mismatch) = state.mismatch_counts();
        assert_eq!(num_match, 2);
        assert_eq!(num_mismatch, 0);
    }

    /// Regression test: a credit applied before its matching debit must not
    /// corrupt the aggregate for mints whose sum sits at `u64::MAX`. With a
    /// saturating accumulator the transient `sum + X` clamps and the excess is
    /// permanently lost (the mainnet false-undercount bug); with wrapping
    /// arithmetic the excess wraps and the debit exactly undoes it.
    #[test]
    fn near_max_supply_mint_survives_credit_before_debit_apply() {
        let mut state = TokenSupplyState::default();
        let mint = Pubkey::new_unique();
        let debtor = Pubkey::new_unique();
        let creditor = Pubkey::new_unique();

        state.upsert_account(&mint, &pack_mint(u64::MAX), None);
        state.upsert_account(&debtor, &pack_token_account(mint, u64::MAX - 5), None);
        state.upsert_account(&creditor, &pack_token_account(mint, 5), None);
        state.seed_mismatched_mints();
        assert!(state.mismatched_mints.is_empty());

        // Transfer 3 from debtor to creditor within one slot; the delta list
        // visits the credited account first (arbitrary order in production).
        let mut dirty = HashSet::new();
        state.upsert_account(&creditor, &pack_token_account(mint, 8), Some(&mut dirty));
        // Transient state wrapped past u64::MAX: (u64::MAX) + 3 ≡ 2 (mod 2^64).
        assert_eq!(
            state.token_amounts_by_mint.read_sync(&mint, |_, v| *v),
            Some(2)
        );
        state.upsert_account(
            &debtor,
            &pack_token_account(mint, u64::MAX - 8),
            Some(&mut dirty),
        );
        state.recheck_dirty_mints(dirty);

        assert_eq!(
            state.token_amounts_by_mint.read_sync(&mint, |_, v| *v),
            Some(u64::MAX)
        );
        assert!(state.mismatched_mints.is_empty());
    }
}
