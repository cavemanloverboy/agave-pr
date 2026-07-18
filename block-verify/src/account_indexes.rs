use {
    solana_accounts_db::accounts_index::{
        AccountIndex, AccountSecondaryIndexes, AccountSecondaryIndexesIncludeExclude,
    },
    solana_pubkey::Pubkey,
    std::collections::HashSet,
};

use crate::verifiers;

/// returns the union of [`BlockVerify::PROGRAM_IDS`](crate::BlockVerify::PROGRAM_IDS) for all
/// registered verifiers (freeze and root tiers).
pub fn registered_program_ids() -> HashSet<Pubkey> {
    verifiers()
        .into_iter()
        .flat_map(|entry| entry.program_ids)
        .collect()
}

/// builds a secondary-index config that enables the program-id index for all registered verifier
/// program ids.
pub fn account_secondary_indexes_for_registered_verifiers() -> AccountSecondaryIndexes {
    merge_account_secondary_indexes(AccountSecondaryIndexes::default())
}

/// merges verifier program-id index requirements into `base`.
pub fn merge_account_secondary_indexes(
    mut base: AccountSecondaryIndexes,
) -> AccountSecondaryIndexes {
    let program_ids = registered_program_ids();
    if program_ids.is_empty() {
        return base;
    }

    base.indexes.insert(AccountIndex::ProgramId);

    match &mut base.keys {
        None => {
            base.keys = Some(AccountSecondaryIndexesIncludeExclude {
                exclude: false,
                keys: program_ids,
            });
        }
        Some(options) if !options.exclude => {
            options.keys.extend(program_ids);
        }
        Some(options) => {
            for program_id in program_ids {
                options.keys.remove(&program_id);
            }
        }
    }

    base
}

#[cfg(test)]
mod tests {
    use {super::*, crate::BlockVerify, solana_runtime::bank::Bank};

    struct IndexVerifier;

    impl BlockVerify for IndexVerifier {
        const PROGRAM_IDS: &'static [Pubkey] = &[Pubkey::new_from_array([9; 32])];

        fn new(_bank: &Bank) -> Self {
            Self
        }

        fn on_account(&mut self, _pubkey: &Pubkey, _account: &solana_account::AccountSharedData) {
        }

        fn after_scan(&self, _bank: &Bank) {}
    }

    #[test]
    fn merge_enables_program_id_index_for_registered_verifiers() {
        crate::register_block_verifier::<IndexVerifier>();

        let merged = merge_account_secondary_indexes(AccountSecondaryIndexes::default());
        assert!(merged.contains(&AccountIndex::ProgramId));
        assert!(merged.include_key(&IndexVerifier::PROGRAM_IDS[0]));
    }

    #[test]
    fn merge_extends_existing_include_keys() {
        crate::register_block_verifier::<IndexVerifier>();

        let existing = Pubkey::new_from_array([1; 32]);
        let base = AccountSecondaryIndexes {
            indexes: HashSet::from([AccountIndex::ProgramId]),
            keys: Some(AccountSecondaryIndexesIncludeExclude {
                exclude: false,
                keys: HashSet::from([existing]),
            }),
        };

        let merged = merge_account_secondary_indexes(base);
        assert!(merged.include_key(&existing));
        assert!(merged.include_key(&IndexVerifier::PROGRAM_IDS[0]));
    }
}
