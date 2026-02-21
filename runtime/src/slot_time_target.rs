use {
    agave_feature_set as feature_set,
    solana_accounts_db::partitioned_rewards::MAX_PARTITIONED_REWARDS_PER_BLOCK,
    solana_cost_model::block_cost_limits::{
        MAX_BLOCK_ACCOUNTS_DATA_SIZE_DELTA, MAX_BLOCK_UNITS, MAX_BLOCK_UNITS_SIMD_0286,
        MAX_VOTE_UNITS, MAX_WRITABLE_ACCOUNT_UNITS,
    },
    solana_pubkey::Pubkey,
};

/// Legacy 400ms slot duration expressed in nanoseconds.
pub(crate) const LEGACY_NS_PER_SLOT: u128 = solana_clock::DEFAULT_MS_PER_SLOT as u128 * 1_000_000;
pub(crate) const LEGACY_HASHES_PER_TICK: u64 = 62_500;
pub(crate) const LEGACY_TARGET_SIGNATURES_PER_SLOT: u64 = 20_000;
pub(crate) const LEGACY_SLOTS_PER_YEAR: f64 = 78_892_314.984;
pub(crate) const LEGACY_MAX_DATA_SHREDS_PER_SLOT: u32 = 32_768;
pub(crate) const LEGACY_MAX_CODE_SHREDS_PER_SLOT: u32 = LEGACY_MAX_DATA_SHREDS_PER_SLOT;
pub(crate) const DEFAULT_MAX_ENTRY_BYTES_PER_SLOT: u64 = 20 * 1024 * 1024; // 20 MiB

/// Slot-duration target and per-stage values from SIMD-0525.
///
/// These are table values, not computed ratios, so validators cannot diverge on
/// rounding at the feature transition.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlotTimeTarget {
    pub(crate) ns_per_slot: u128,
    pub(crate) slots_per_year: f64,
    pub(crate) hashes_per_tick: u64,
    pub(crate) target_signatures_per_slot: u64,
    pub(crate) max_block_units: u64,
    pub(crate) max_writable_account_units: u64,
    pub(crate) max_block_units_simd_0286: u64,
    pub(crate) max_writable_account_units_simd_0286: u64,
    pub(crate) max_vote_units: u64,
    pub(crate) max_block_accounts_data_size_delta: u64,
    pub(crate) max_data_shreds_per_slot: u32,
    pub(crate) max_code_shreds_per_slot: u32,
    pub(crate) max_entry_bytes_per_slot: u64,
    pub(crate) partitioned_epoch_rewards_stake_account_stores_per_block: u64,
}

impl SlotTimeTarget {
    /// Target nanoseconds per slot for this slot-time stage.
    pub const fn ns_per_slot(&self) -> u128 {
        self.ns_per_slot
    }

    /// PoH hashes per tick for this slot-time stage.
    pub const fn hashes_per_tick(&self) -> u64 {
        self.hashes_per_tick
    }

    /// Maximum data shred index capacity for this slot-time stage.
    pub const fn max_data_shreds_per_slot(&self) -> u32 {
        self.max_data_shreds_per_slot
    }

    /// Maximum coding shred index capacity for this slot-time stage.
    pub const fn max_code_shreds_per_slot(&self) -> u32 {
        self.max_code_shreds_per_slot
    }

    /// Returns the per-bank cost limits for this slot-time stage.
    pub(crate) const fn cost_limits(
        self,
        raise_block_limits_to_100m: bool,
    ) -> (u64, u64, u64, u64) {
        if raise_block_limits_to_100m {
            (
                self.max_writable_account_units_simd_0286,
                self.max_block_units_simd_0286,
                self.max_vote_units,
                self.max_block_accounts_data_size_delta,
            )
        } else {
            (
                self.max_writable_account_units,
                self.max_block_units,
                self.max_vote_units,
                self.max_block_accounts_data_size_delta,
            )
        }
    }
}

pub(crate) const LEGACY_SLOT_TIME_TARGET: SlotTimeTarget = SlotTimeTarget {
    ns_per_slot: LEGACY_NS_PER_SLOT,
    slots_per_year: LEGACY_SLOTS_PER_YEAR,
    hashes_per_tick: LEGACY_HASHES_PER_TICK,
    target_signatures_per_slot: LEGACY_TARGET_SIGNATURES_PER_SLOT,
    max_block_units: MAX_BLOCK_UNITS,
    max_writable_account_units: MAX_WRITABLE_ACCOUNT_UNITS,
    max_block_units_simd_0286: MAX_BLOCK_UNITS_SIMD_0286,
    max_writable_account_units_simd_0286: 40_000_000,
    max_vote_units: MAX_VOTE_UNITS,
    max_block_accounts_data_size_delta: MAX_BLOCK_ACCOUNTS_DATA_SIZE_DELTA,
    max_data_shreds_per_slot: LEGACY_MAX_DATA_SHREDS_PER_SLOT,
    max_code_shreds_per_slot: LEGACY_MAX_CODE_SHREDS_PER_SLOT,
    max_entry_bytes_per_slot: DEFAULT_MAX_ENTRY_BYTES_PER_SLOT,
    partitioned_epoch_rewards_stake_account_stores_per_block: MAX_PARTITIONED_REWARDS_PER_BLOCK,
};
pub(crate) const SLOT_TIME_TARGET_350MS: SlotTimeTarget = SlotTimeTarget {
    ns_per_slot: 350_000_000,
    slots_per_year: 90_162_645.696,
    hashes_per_tick: 54_687,
    target_signatures_per_slot: 17_500,
    max_block_units: 52_500_000,
    max_writable_account_units: 21_000_000,
    max_block_units_simd_0286: 87_500_000,
    max_writable_account_units_simd_0286: 35_000_000,
    max_vote_units: 31_500_000,
    max_block_accounts_data_size_delta: 87_500_000,
    max_data_shreds_per_slot: 28_672,
    max_code_shreds_per_slot: 28_672,
    max_entry_bytes_per_slot: 18_350_080,
    partitioned_epoch_rewards_stake_account_stores_per_block: 3_584,
};
pub(crate) const SLOT_TIME_TARGET_300MS: SlotTimeTarget = SlotTimeTarget {
    ns_per_slot: 300_000_000,
    slots_per_year: 105_189_753.312,
    hashes_per_tick: 46_875,
    target_signatures_per_slot: 15_000,
    max_block_units: 45_000_000,
    max_writable_account_units: 18_000_000,
    max_block_units_simd_0286: 75_000_000,
    max_writable_account_units_simd_0286: 30_000_000,
    max_vote_units: 27_000_000,
    max_block_accounts_data_size_delta: 75_000_000,
    max_data_shreds_per_slot: 24_576,
    max_code_shreds_per_slot: 24_576,
    max_entry_bytes_per_slot: 15_728_640,
    partitioned_epoch_rewards_stake_account_stores_per_block: 3_072,
};
pub(crate) const SLOT_TIME_TARGET_250MS: SlotTimeTarget = SlotTimeTarget {
    ns_per_slot: 250_000_000,
    slots_per_year: 126_227_703.974,
    hashes_per_tick: 39_062,
    target_signatures_per_slot: 12_500,
    max_block_units: 37_500_000,
    max_writable_account_units: 15_000_000,
    max_block_units_simd_0286: 62_500_000,
    max_writable_account_units_simd_0286: 25_000_000,
    max_vote_units: 22_500_000,
    max_block_accounts_data_size_delta: 62_500_000,
    max_data_shreds_per_slot: 20_480,
    max_code_shreds_per_slot: 20_480,
    max_entry_bytes_per_slot: 13_107_200,
    partitioned_epoch_rewards_stake_account_stores_per_block: 2_560,
};
pub(crate) const SLOT_TIME_TARGET_200MS: SlotTimeTarget = SlotTimeTarget {
    ns_per_slot: 200_000_000,
    slots_per_year: 157_784_629.968,
    hashes_per_tick: 31_250,
    target_signatures_per_slot: 10_000,
    max_block_units: 30_000_000,
    max_writable_account_units: 12_000_000,
    max_block_units_simd_0286: 50_000_000,
    max_writable_account_units_simd_0286: 20_000_000,
    max_vote_units: 18_000_000,
    max_block_accounts_data_size_delta: 50_000_000,
    max_data_shreds_per_slot: 16_384,
    max_code_shreds_per_slot: 16_384,
    max_entry_bytes_per_slot: 10_485_760,
    partitioned_epoch_rewards_stake_account_stores_per_block: 2_048,
};

/// Slot-time reduction gates in the intended activation order.
pub const SLOT_TIME_REDUCTION_TARGETS: [(Pubkey, SlotTimeTarget); 4] = [
    (
        feature_set::reduce_slot_time_to_350ms::ID,
        SLOT_TIME_TARGET_350MS,
    ),
    (
        feature_set::reduce_slot_time_to_300ms::ID,
        SLOT_TIME_TARGET_300MS,
    ),
    (
        feature_set::reduce_slot_time_to_250ms::ID,
        SLOT_TIME_TARGET_250MS,
    ),
    (
        feature_set::reduce_slot_time_to_200ms::ID,
        SLOT_TIME_TARGET_200MS,
    ),
];

/// Returns slot-time feature gates mapped to runtime slot-time targets.
///
/// Runtime owns the SIMD-0525 table values. `feature-set` owns only the feature
/// IDs, while this table defines the effective runtime behavior for each ID.
pub fn slot_time_feature_gates() -> [(Pubkey, SlotTimeTarget); 4] {
    SLOT_TIME_REDUCTION_TARGETS
}

/// Returns all slot-time reduction feature IDs in activation order.
///
/// Tests and genesis helpers use this when they need to disable the whole
/// staged slot-time feature set without duplicating the feature table.
pub fn slot_time_feature_ids() -> [Pubkey; 4] {
    SLOT_TIME_REDUCTION_TARGETS.map(|(feature_id, _)| feature_id)
}
