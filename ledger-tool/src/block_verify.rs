use {
    crate::{args::parse_process_options, ledger_utils::*},
    agave_block_verify,
    clap::{App, Arg, ArgMatches, SubCommand},
    log::*,
    std::{path::Path, sync::Arc},
};

pub fn block_verify_subcommand<'a, 'b>(
    load_genesis_config_arg: &Arg<'a, 'b>,
    accounts_db_config_args: &[Arg<'a, 'b>],
    snapshot_config_args: &[Arg<'a, 'b>],
    halt_at_slot_arg: &Arg<'a, 'b>,
) -> App<'a, 'b> {
    SubCommand::with_name("block-verify")
        .about("Run block verifiers against a snapshot bank")
        .long_about(
            "Loads a snapshot bank and runs registered block verifiers (e.g. SPL token total \
             supply checks). By default stops at the snapshot slot without replaying additional \
             blocks. Wrapped SOL (native mint) mismatches are expected and can be ignored.",
        )
        .arg(load_genesis_config_arg)
        .args(accounts_db_config_args)
        .args(snapshot_config_args)
        .arg(halt_at_slot_arg)
}

pub fn block_verify_command(ledger_path: &Path, arg_matches: &ArgMatches<'_>) {
    agave_block_verify::install_default_block_verifiers();

    let mut process_options = parse_process_options(ledger_path, arg_matches);
    if process_options.halt_at_slot.is_none() {
        process_options.halt_at_slot = Some(0);
    }
    process_options.accounts_db_config.account_indexes = Some(
        agave_block_verify::merge_account_secondary_indexes(
            process_options
                .accounts_db_config
                .account_indexes
                .take()
                .unwrap_or_default(),
        ),
    );

    let genesis_config = open_genesis_config_by(ledger_path, arg_matches);
    let blockstore = open_blockstore(
        ledger_path,
        arg_matches,
        get_access_type(&process_options),
    );
    let LoadAndProcessLedgerOutput { bank_forks, .. } = load_and_process_ledger_or_exit(
        arg_matches,
        &genesis_config,
        Arc::new(blockstore),
        process_options,
        None,
    );

    let bank = bank_forks.read().unwrap().working_bank();
    info!("Running block verifiers at slot {}", bank.slot());
    // Blocking shared scan: incremental self-verification would only schedule a
    // background rebuild that a one-shot tool never waits for.
    agave_block_verify::run_registered_block_verifiers_blocking(&bank);
}
