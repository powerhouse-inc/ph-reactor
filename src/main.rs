//! `ph-reactor` entry point: parse arguments, dispatch to the daemon.

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use ph_reactor as ph;

fn main() -> ExitCode {
    let args = ph::cli::CliArgs::parse();
    let state_dir = args.state_dir.as_deref();

    // The daemonized start forks before any tokio runtime exists (the
    // child builds a fresh one); every other command runs on one here.
    if matches!(
        args.command,
        Some(ph::cli::Command::Run { daemonize: true })
    ) {
        match ph::daemon::run_daemonized(state_dir) {
            Ok(code) => return ExitCode::from(code as u8),
            Err(err) => {
                eprintln!("ph-reactor: {err:#}");
                if let Some(cause) = err.source() {
                    eprintln!("  caused by: {cause}");
                }
                return ExitCode::FAILURE;
            }
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime builds");
    match rt.block_on(dispatch(args.command, state_dir)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ph-reactor: {err:#}");
            if let Some(cause) = err.source() {
                eprintln!("  caused by: {cause}");
            }
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(
    command: Option<ph::cli::Command>,
    state_dir: Option<&std::path::Path>,
) -> Result<()> {
    match command.unwrap_or(ph::cli::Command::Run { daemonize: false }) {
        ph::cli::Command::Run { daemonize } => ph::daemon::run(state_dir, daemonize).await,
        ph::cli::Command::Stop => ph::daemon::stop(state_dir).await,
        ph::cli::Command::Status { json } => ph::daemon::status(state_dir, json).await,
        ph::cli::Command::Drive(cmd) => ph::daemon::drive_command(state_dir, cmd).await,
        ph::cli::Command::Doctor => ph::daemon::doctor(state_dir).await,
        ph::cli::Command::Config(cmd) => config_command(state_dir, cmd).await,
        ph::cli::Command::Logs {
            follow,
            switchboard,
        } => ph::daemon::logs(state_dir, follow, switchboard).await,
    }
}

/// `config` subcommands work without a running daemon: they read and write
/// the state file directly.
async fn config_command(
    state_dir: Option<&std::path::Path>,
    command: ph::cli::ConfigCommand,
) -> Result<()> {
    use ph::cli::ConfigCommand;
    let paths = ph::paths::StatePaths::resolve(state_dir);
    paths.ensure_dirs()?;
    match command {
        ConfigCommand::Show => {
            let config = ph::config::load(&paths)?;
            print!("{}", ph::config::render(&config));
            Ok(())
        }
        ConfigCommand::Set { key, value } => {
            let mut config = ph::config::load(&paths)?;
            let value: serde_json::Value = match serde_json::from_str(&value) {
                Ok(v) => v,
                Err(_) => serde_json::Value::String(value),
            };
            ph::config::set(&mut config, &key, &value)?;
            ph::config::save(&paths, &config)?;
            println!("set {key}");
            Ok(())
        }
    }
}
