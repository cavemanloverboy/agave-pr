use {
    crate::{
        BlockVerify,
        chain_deltas::{AccountDelta, PendingSlotDelta, WarmupWindow, buffer_chain_deltas,
            collect_parent_chain_deltas},
    },
    log::{error, info, warn},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::{
        accounts::Accounts, accounts_index::IndexKey, ancestors::Ancestors,
        is_loadable::IsLoadable as _,
    },
    solana_clock::{BankId, Slot},
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    std::{
        collections::{HashMap, VecDeque},
        sync::{Arc, Mutex},
        thread,
        time::Instant,
    },
};

const EXPONENT_CORE_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ExponentnaRg3CQbW6dqQNZKXp7gtZ9DGMp1cwC4HAS7");

// Anchor account discriminators: sha256("account:<name>")[0..8].
const VAULT_DISCRIMINATOR: [u8; 8] = [211, 8, 232, 43, 2, 152, 117, 119];
const YIELD_TOKEN_POSITION_DISCRIMINATOR: [u8; 8] = [227, 92, 146, 49, 29, 85, 71, 94];

// Vault offsets after the 8-byte Anchor discriminator.
const VAULT_MINT_YT_OFFSET: usize = 72;
const VAULT_ESCROW_YT_OFFSET: usize = 136;
const VAULT_YIELD_POSITION_OFFSET: usize = 200;
const VAULT_START_TS_OFFSET: usize = 264;
const VAULT_DURATION_OFFSET: usize = 268;
const VAULT_MIN_LEN: usize = VAULT_DURATION_OFFSET + 4;

// YieldTokenPosition offsets after the 8-byte Anchor discriminator.
const YTP_VAULT_OFFSET: usize = 40;
const YTP_YT_BALANCE_OFFSET: usize = 72;
const YTP_MIN_LEN: usize = YTP_YT_BALANCE_OFFSET + 8;

const SPL_TOKEN_ACCOUNT_LEN: usize = 165;
const TOKEN_ACCOUNT_MINT_OFFSET: usize = 0;
const TOKEN_ACCOUNT_AMOUNT_OFFSET: usize = 64;
const TOKEN_ACCOUNT_STATE_OFFSET: usize = 108;
const TOKEN_ACCOUNT_INITIALIZED_STATE: u8 = 1;

const FULL_SCAN_PROGRESS_INTERVAL: usize = 100_000;
/// Max per-slot account-update batches buffered while a background rebuild runs.
const PENDING_SLOT_CAP: usize = 8192;

#[derive(Debug, Clone)]
struct VaultLite {
    mint_yt: Pubkey,
    escrow_yt: Pubkey,
    yield_position: Pubkey,
    start_ts: u32,
    duration: u32,
}

impl VaultLite {
    fn is_active(&self, now_ts: i64) -> bool {
        let start_ts = i64::from(self.start_ts);
        let end_ts = start_ts + i64::from(self.duration);
        start_ts <= now_ts && now_ts <= end_ts
    }
}

#[derive(Debug, Clone, Copy)]
struct YieldTokenPositionLite {
    vault: Pubkey,
    yt_balance: u64,
}

#[derive(Debug, Clone, Copy)]
struct TokenAccountLite {
    mint: Pubkey,
    amount: u64,
    state: u8,
}

#[derive(Debug, Default)]
struct CoreYtState {
    slot: Slot,
    vaults: HashMap<Pubkey, VaultLite>,
    yt_sum_by_vault: HashMap<Pubkey, u128>,
    positions: HashMap<Pubkey, YieldTokenPositionLite>,
    malformed_core_accounts: usize,
}

impl CoreYtState {
    fn clear_account(&mut self, pubkey: &Pubkey) {
        self.vaults.remove(pubkey);
        if let Some(position) = self.positions.remove(pubkey) {
            if let Some(total) = self.yt_sum_by_vault.get_mut(&position.vault) {
                *total = total.saturating_sub(u128::from(position.yt_balance));
                if *total == 0 {
                    self.yt_sum_by_vault.remove(&position.vault);
                }
            }
        }
    }

    fn upsert_account(&mut self, pubkey: &Pubkey, account: &AccountSharedData) {
        self.clear_account(pubkey);

        if !account.is_loadable() || account.owner() != &EXPONENT_CORE_PROGRAM_ID {
            return;
        }

        let data = account.data();
        let Some(discriminator) = data.get(0..8) else {
            self.malformed_core_accounts += 1;
            return;
        };

        if discriminator == VAULT_DISCRIMINATOR {
            match parse_vault(data) {
                Some(vault) => {
                    self.vaults.insert(*pubkey, vault);
                }
                None => {
                    self.malformed_core_accounts += 1;
                }
            }
            return;
        }

        if discriminator == YIELD_TOKEN_POSITION_DISCRIMINATOR {
            match parse_yield_token_position(data) {
                Some(position) => {
                    let total = self.yt_sum_by_vault.entry(position.vault).or_default();
                    *total = total
                        .checked_add(u128::from(position.yt_balance))
                        .expect("u128 YT balance accumulator overflow");
                    self.positions.insert(*pubkey, position);
                }
                None => {
                    self.malformed_core_accounts += 1;
                }
            }
        }
    }

    fn tracks(&self, pubkey: &Pubkey) -> bool {
        self.vaults.contains_key(pubkey) || self.positions.contains_key(pubkey)
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
        /// Highest confirmed slot whose deltas have been buffered; buffering
        /// resumes from here rather than `pending.back()` so draining
        /// `pending` cannot re-anchor the walk at an already-rooted slot.
        buffered_through: Slot,
        pending: VecDeque<PendingSlotDelta>,
        invalidated: bool,
    },
    Ready(CoreYtState),
}

static STATE_CACHE: Mutex<CacheState> = Mutex::new(CacheState::Cold);

/// Confirmed-tier verify: background full rebuild on first confirmed bank, then
/// per-slot catch-up along the confirmed chain.
///
/// Called once per optimistically confirmed bank, in ascending slot order, on the
/// tracker thread. Confirmed banks do not revert and their parent chains are
/// live, so gaps are healed by walking `Bank::parent()` from the last verified
/// slot instead of discarding the cache.
pub fn verify_core_yt_balance(bank: &Arc<Bank>) {
    let started = Instant::now();
    let mut cache = STATE_CACHE.lock().unwrap();

    match &mut *cache {
        CacheState::Ready(state) if state.slot >= bank.slot() => {
            // Confirmed notifications arrive in ascending order; this is defensive.
            info!(
                "core yt balance verifier: slot={} mode=already_confirmed cache_slot={}",
                bank.slot(),
                state.slot,
            );
        }
        CacheState::Ready(state) => {
            let deltas = match collect_parent_chain_deltas(state.slot, bank) {
                Ok(deltas) => deltas,
                Err(reason) => {
                    warn!(
                        "core yt balance verifier: catch_up unavailable at slot={} ({reason}); \
                         deferring full rebuild until confirmed-slot progression is steady",
                        bank.slot(),
                    );
                    *cache = CacheState::Warming(WarmupWindow::new(bank.slot()));
                    return;
                }
            };
            let mut modified = 0usize;
            for (slot, slot_deltas) in deltas {
                modified += apply_delta_slice(state, &slot_deltas);
                state.slot = slot;
            }
            state.slot = bank.slot();
            let scan_us = started.elapsed().as_micros();
            let check_started = Instant::now();
            let now_ts = bank.clock().unix_timestamp;
            let active_checked = state
                .vaults
                .values()
                .filter(|vault| vault.is_active(now_ts))
                .count();
            let failing = failing_vaults(state, bank);
            let check_us = check_started.elapsed().as_micros();

            info!(
                "core yt balance verifier: slot={} mode=catch_up modified={modified} \
                 scan_us={scan_us} check_us={check_us} vaults={} active_checked={active_checked} \
                 positions={} malformed_core_accounts={} failures={}",
                bank.slot(),
                state.vaults.len(),
                state.positions.len(),
                state.malformed_core_accounts,
                failing.len()
            );

            for failure in &failing {
                warn!("core yt balance mismatch: {failure}");
            }
            if !failing.is_empty() {
                error!(
                    "core yt balance invariant failed for {} vault(s)",
                    failing.len()
                );
            }
        }
        CacheState::Building {
            base_slot,
            buffered_through,
            pending,
            invalidated,
        } => {
            buffer_chain_deltas(
                "core yt balance verifier",
                bank,
                *buffered_through,
                pending,
                invalidated,
                PENDING_SLOT_CAP,
            );
            if !*invalidated {
                *buffered_through = bank.slot();
            }
            if pending.len() % 1000 == 0 && !pending.is_empty() {
                info!(
                    "core yt balance verifier: rebuild in progress base_slot={base_slot} \
                     buffered_slots={} tip_slot={}",
                    pending.len(),
                    bank.slot(),
                );
            }
        }
        CacheState::Warming(window) => {
            if window.observe("core yt balance verifier", bank.slot()) {
                start_background_rebuild(bank, &mut cache);
            }
        }
        CacheState::Cold => {
            *cache = CacheState::Warming(WarmupWindow::new(bank.slot()));
        }
    }
}

fn start_background_rebuild(bank: &Bank, cache: &mut CacheState) {
    let base_slot = bank.slot();
    *cache = CacheState::Building {
        base_slot,
        buffered_through: base_slot,
        pending: VecDeque::new(),
        invalidated: false,
    };

    let accounts = Arc::clone(&bank.rc.accounts);
    let ancestors = bank.ancestors.clone();
    let bank_id = bank.bank_id();

    info!(
        "core yt balance verifier: scheduling background full rebuild slot={base_slot} \
         (replay continues)"
    );

    let spawn_result = thread::Builder::new()
        .name("core-yt-rebuild".into())
        .spawn(move || finish_background_rebuild(accounts, ancestors, bank_id, base_slot));

    if let Err(err) = spawn_result {
        warn!("core yt balance verifier: failed to spawn rebuild thread: {err}");
        *cache = CacheState::Cold;
    }
}

fn finish_background_rebuild(
    accounts: Arc<Accounts>,
    ancestors: Ancestors,
    bank_id: BankId,
    base_slot: Slot,
) {
    let result = rebuild_state(&accounts, &ancestors, bank_id, base_slot);
    let mut cache = STATE_CACHE.lock().unwrap();
    let install = {
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
            None
        } else {
            match result {
                Ok(mut state) => {
                    let mut applied_slots = 0usize;
                    let mut modified = 0usize;
                    for (slot, deltas) in pending.drain(..) {
                        modified += apply_delta_slice(&mut state, &deltas);
                        state.slot = slot;
                        applied_slots += 1;
                    }
                    Some(Ok((state, applied_slots, modified)))
                }
                Err(err) => Some(Err(err)),
            }
        }
    };

    match install {
        None => {
            warn!(
                "core yt balance verifier: discarding rebuild for slot={base_slot} after fork/gap"
            );
            *cache = CacheState::Cold;
        }
        Some(Ok((state, applied_slots, modified))) => {
            info!(
                "core yt balance verifier: slot={} mode=full_background \
                 applied_slots={applied_slots} modified={modified} vaults={} positions={} \
                 malformed_core_accounts={}",
                state.slot,
                state.vaults.len(),
                state.positions.len(),
                state.malformed_core_accounts,
            );
            *cache = CacheState::Ready(state);
        }
        Some(Err(err)) => {
            warn!(
                "core yt balance verifier: background full scan failed for slot {base_slot}: {err}"
            );
            *cache = CacheState::Cold;
        }
    }
}

fn apply_delta_slice(state: &mut CoreYtState, deltas: &[AccountDelta]) -> usize {
    let mut touched = 0usize;
    for (pubkey, account) in deltas {
        let owned = account.is_loadable() && account.owner() == &EXPONENT_CORE_PROGRAM_ID;
        if owned || state.tracks(pubkey) {
            state.upsert_account(pubkey, account);
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
) -> Result<CoreYtState, String> {
    let mut state = CoreYtState {
        slot,
        ..CoreYtState::default()
    };
    let mut fetched = 0usize;
    let started = Instant::now();

    info!("core yt balance verifier: full rebuild starting slot={slot}");

    accounts
        .scan_by_index_key_with_filter(
            ancestors,
            bank_id,
            &IndexKey::ProgramId(EXPONENT_CORE_PROGRAM_ID),
            |account| account.is_loadable() && account.owner() == &EXPONENT_CORE_PROGRAM_ID,
            |pubkey, account| {
                fetched += 1;
                if fetched % FULL_SCAN_PROGRESS_INTERVAL == 0 {
                    info!("core yt balance verifier: full rebuild slot={slot} fetched={fetched}");
                }
                state.upsert_account(pubkey, &account);
            },
        )
        .map_err(|err| err.to_string())?;

    // If the program-id index is not configured, indexed scan yields nothing; fall back.
    if fetched == 0 {
        accounts
            .scan_all(ancestors, bank_id, |account_tuple| {
                let Some((pubkey, account, _slot)) = account_tuple else {
                    return;
                };
                if !account.is_loadable() || account.owner() != &EXPONENT_CORE_PROGRAM_ID {
                    return;
                }
                fetched += 1;
                if fetched % FULL_SCAN_PROGRESS_INTERVAL == 0 {
                    info!("core yt balance verifier: full rebuild slot={slot} fetched={fetched}");
                }
                state.upsert_account(pubkey, &account);
            })
            .map_err(|err| err.to_string())?;
    }

    info!(
        "core yt balance verifier: full rebuild done slot={slot} fetched={fetched} scan_us={}",
        started.elapsed().as_micros()
    );
    Ok(state)
}

fn failing_vaults(state: &CoreYtState, bank: &Bank) -> Vec<String> {
    let now_ts = bank.clock().unix_timestamp;
    let mut failing_vaults = Vec::new();

    for (vault_key, vault) in &state.vaults {
        if !vault.is_active(now_ts) {
            continue;
        }

        let total_yt = state.yt_sum_by_vault.get(vault_key).copied().unwrap_or(0);
        let Some(escrow_yt) = state
            .positions
            .get(&vault.yield_position)
            .map(|position| position.yt_balance)
        else {
            failing_vaults.push(format!(
                "vault={vault_key} missing_yield_position={}",
                vault.yield_position
            ));
            continue;
        };

        let Some(user_claims) = total_yt.checked_sub(u128::from(escrow_yt)) else {
            failing_vaults.push(format!(
                "vault={vault_key} escrow_position_balance={escrow_yt} total_position_balance={total_yt}"
            ));
            continue;
        };

        match load_spl_token_account(bank, &vault.escrow_yt) {
            Ok(escrow) => {
                if escrow.state != TOKEN_ACCOUNT_INITIALIZED_STATE {
                    failing_vaults.push(format!(
                        "vault={vault_key} escrow={} invalid_state={}",
                        vault.escrow_yt, escrow.state
                    ));
                    continue;
                }

                if escrow.mint != vault.mint_yt {
                    failing_vaults.push(format!(
                        "vault={vault_key} escrow={} mint_mismatch expected={} actual={}",
                        vault.escrow_yt, vault.mint_yt, escrow.mint
                    ));
                    continue;
                }

                let escrow_amount = u128::from(escrow.amount);
                if user_claims > escrow_amount {
                    failing_vaults.push(format!(
                        "vault={vault_key} claims_raw={user_claims} escrow_raw={escrow_amount} shortfall_raw={}",
                        user_claims - escrow_amount
                    ));
                }
            }
            Err(error) => {
                failing_vaults.push(format!(
                    "vault={vault_key} escrow={} error={error}",
                    vault.escrow_yt
                ));
            }
        }
    }

    failing_vaults
}

/// Legacy [`BlockVerify`] adapter used by tests / [`crate::run_block_verifier`].
#[derive(Debug, Default)]
pub struct CoreYtBalanceVerifier {
    state: CoreYtState,
}

impl BlockVerify for CoreYtBalanceVerifier {
    const PROGRAM_IDS: &'static [Pubkey] = &[EXPONENT_CORE_PROGRAM_ID];

    fn try_self_verify(bank: &Arc<Bank>) -> bool {
        verify_core_yt_balance(bank);
        true
    }

    fn new(bank: &Bank) -> Self {
        Self {
            state: CoreYtState {
                slot: bank.slot(),
                ..CoreYtState::default()
            },
        }
    }

    fn on_account(&mut self, pubkey: &Pubkey, account: &AccountSharedData) {
        // Shared-scan path visits each account once; insert without clear-first.
        if account.owner() != &EXPONENT_CORE_PROGRAM_ID {
            return;
        }
        let data = account.data();
        let Some(discriminator) = data.get(0..8) else {
            self.state.malformed_core_accounts += 1;
            return;
        };
        if discriminator == VAULT_DISCRIMINATOR {
            match parse_vault(data) {
                Some(vault) => {
                    self.state.vaults.insert(*pubkey, vault);
                }
                None => self.state.malformed_core_accounts += 1,
            }
            return;
        }
        if discriminator == YIELD_TOKEN_POSITION_DISCRIMINATOR {
            match parse_yield_token_position(data) {
                Some(position) => {
                    let total = self.state.yt_sum_by_vault.entry(position.vault).or_default();
                    *total = total
                        .checked_add(u128::from(position.yt_balance))
                        .expect("u128 YT balance accumulator overflow");
                    self.state.positions.insert(*pubkey, position);
                }
                None => self.state.malformed_core_accounts += 1,
            }
        }
    }

    fn after_scan(&self, bank: &Bank) {
        let failing = failing_vaults(&self.state, bank);
        info!(
            "core yt balance verifier: slot={} mode=legacy vaults={} positions={} failures={}",
            bank.slot(),
            self.state.vaults.len(),
            self.state.positions.len(),
            failing.len()
        );
        for failure in &failing {
            warn!("core yt balance mismatch: {failure}");
        }
        if !failing.is_empty() {
            error!(
                "core yt balance invariant failed for {} vault(s)",
                failing.len()
            );
        }
    }
}

fn parse_vault(data: &[u8]) -> Option<VaultLite> {
    if data.len() < VAULT_MIN_LEN {
        return None;
    }

    Some(VaultLite {
        mint_yt: read_pubkey(data, VAULT_MINT_YT_OFFSET)?,
        escrow_yt: read_pubkey(data, VAULT_ESCROW_YT_OFFSET)?,
        yield_position: read_pubkey(data, VAULT_YIELD_POSITION_OFFSET)?,
        start_ts: read_u32_le(data, VAULT_START_TS_OFFSET)?,
        duration: read_u32_le(data, VAULT_DURATION_OFFSET)?,
    })
}

fn parse_yield_token_position(data: &[u8]) -> Option<YieldTokenPositionLite> {
    if data.len() < YTP_MIN_LEN {
        return None;
    }

    Some(YieldTokenPositionLite {
        vault: read_pubkey(data, YTP_VAULT_OFFSET)?,
        yt_balance: read_u64_le(data, YTP_YT_BALANCE_OFFSET)?,
    })
}

fn load_spl_token_account(bank: &Bank, address: &Pubkey) -> Result<TokenAccountLite, String> {
    let account = bank
        .get_account(address)
        .ok_or_else(|| "missing escrow token account".to_string())?;

    if account.owner() != &spl_token_interface::ID {
        return Err(format!("owner_mismatch owner={}", account.owner()));
    }

    parse_spl_token_account(account.data()).ok_or_else(|| "invalid token account data".to_string())
}

fn parse_spl_token_account(data: &[u8]) -> Option<TokenAccountLite> {
    if data.len() < SPL_TOKEN_ACCOUNT_LEN {
        return None;
    }

    Some(TokenAccountLite {
        mint: read_pubkey(data, TOKEN_ACCOUNT_MINT_OFFSET)?,
        amount: read_u64_le(data, TOKEN_ACCOUNT_AMOUNT_OFFSET)?,
        state: *data.get(TOKEN_ACCOUNT_STATE_OFFSET)?,
    })
}

fn read_pubkey(data: &[u8], offset: usize) -> Option<Pubkey> {
    let bytes: [u8; 32] = data.get(offset..offset + 32)?.try_into().ok()?;
    Some(Pubkey::new_from_array(bytes))
}

fn read_u32_le(data: &[u8], offset: usize) -> Option<u32> {
    let bytes: [u8; 4] = data.get(offset..offset + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn read_u64_le(data: &[u8], offset: usize) -> Option<u64> {
    let bytes: [u8; 8] = data.get(offset..offset + 8)?.try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position_account(vault: &Pubkey, yt_balance: u64) -> AccountSharedData {
        let mut data = vec![0u8; YTP_MIN_LEN];
        data[0..8].copy_from_slice(&YIELD_TOKEN_POSITION_DISCRIMINATOR);
        data[YTP_VAULT_OFFSET..YTP_VAULT_OFFSET + 32].copy_from_slice(vault.as_ref());
        data[YTP_YT_BALANCE_OFFSET..YTP_YT_BALANCE_OFFSET + 8]
            .copy_from_slice(&yt_balance.to_le_bytes());
        let mut account = AccountSharedData::new(1, YTP_MIN_LEN, &EXPONENT_CORE_PROGRAM_ID);
        account.set_data_from_slice(&data);
        account
    }

    #[test]
    fn upsert_and_clear_adjusts_vault_sums() {
        let mut state = CoreYtState::default();
        let vault = Pubkey::new_unique();
        let position = Pubkey::new_unique();

        state.upsert_account(&position, &position_account(&vault, 100));
        assert_eq!(state.yt_sum_by_vault.get(&vault), Some(&100));

        state.upsert_account(&position, &position_account(&vault, 40));
        assert_eq!(state.yt_sum_by_vault.get(&vault), Some(&40));

        state.upsert_account(&position, &AccountSharedData::default());
        assert!(!state.positions.contains_key(&position));
        assert!(!state.yt_sum_by_vault.contains_key(&vault));
    }
}
