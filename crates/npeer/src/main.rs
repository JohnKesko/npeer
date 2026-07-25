//! npeer: a fast, friendly BitTorrent client for the command line.

mod cli;
mod commands;
mod config;
mod ui;
mod units;

use std::process::ExitCode;

use anyhow::Result;
use clap::{CommandFactory, Parser};

use crate::cli::{Cli, Command, ConfigAction};
use crate::commands::Ctx;
use crate::config::Overrides;

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("error: could not start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(cli)) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let config_arg = cli.config.as_deref();

    // No subcommand (plain `npeer`, or stray flags like `npeer -v`):
    // show the help screen instead of a terse usage error.
    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        return Ok(ExitCode::from(2));
    };

    // Commands that do not need resolved settings.
    match &command {
        Command::Decode { value } => {
            commands::decode::run(value.as_deref())?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Completions { shell } => {
            let mut command = Cli::command();
            clap_complete::generate(*shell, &mut command, "npeer", &mut std::io::stdout());
            return Ok(ExitCode::SUCCESS);
        }
        #[cfg(feature = "upgrade")]
        Command::Upgrade { check, yes } => {
            // Deliberately independent of the config: upgrading must work
            // even when the config file is broken.
            return commands::upgrade::run(config::ProgressMode::Auto, *check, *yes).await;
        }
        Command::Config { action } => {
            match action {
                ConfigAction::Init { force } => commands::config_cmd::init(config_arg, *force)?,
                ConfigAction::Path => commands::config_cmd::path(config_arg)?,
                ConfigAction::Validate => commands::config_cmd::validate(config_arg)?,
                ConfigAction::Show => {
                    let (file, path) = config::load_file(config_arg)?;
                    let settings = config::resolve(&file, &Overrides::default(), env_lookup)?;
                    commands::config_cmd::show(&settings, &path)?;
                }
            }
            return Ok(ExitCode::SUCCESS);
        }
        _ => {}
    }

    // Everything else runs with resolved settings.
    let (file, _path) = config::load_file(config_arg)?;
    let overrides = match &command {
        Command::Download(args) => args.tuning.to_overrides(),
        Command::Resume(args) => args.tuning.to_overrides(),
        _ => Overrides::default(),
    };
    let settings = config::resolve(&file, &overrides, env_lookup)?;
    let ctx = Ctx::new(settings)?;

    let code = match command {
        Command::Download(args) => {
            commands::download::run(&ctx, &args.sources, args.tuning.output.clone()).await?
        }
        Command::Resume(args) => {
            commands::download::resume(&ctx, args.id.as_deref(), args.all).await?
        }
        Command::Info { source } => {
            commands::info::run(&ctx, &source).await?;
            ExitCode::SUCCESS
        }
        Command::Peers { source } => {
            commands::peers::run(&ctx, &source).await?;
            ExitCode::SUCCESS
        }
        Command::List => {
            commands::list::list(&ctx)?;
            ExitCode::SUCCESS
        }
        Command::Status { id } => {
            commands::list::status(&ctx, id.as_deref())?;
            ExitCode::SUCCESS
        }
        Command::Pause { id } => {
            commands::manage::pause(&ctx, &id)?;
            ExitCode::SUCCESS
        }
        Command::Stop { id } => {
            commands::manage::stop(&ctx, &id)?;
            ExitCode::SUCCESS
        }
        Command::Remove {
            id,
            delete_files,
            yes,
        } => {
            commands::manage::remove(&ctx, &id, delete_files, yes)?;
            ExitCode::SUCCESS
        }
        Command::Verify { id } => {
            commands::verify::run(&ctx, &id).await?;
            ExitCode::SUCCESS
        }
        Command::Decode { .. } | Command::Completions { .. } | Command::Config { .. } => {
            unreachable!("handled above")
        }
        #[cfg(feature = "upgrade")]
        Command::Upgrade { .. } => unreachable!("handled above"),
    };
    Ok(code)
}

fn env_lookup(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// `-v` info, `-vv` debug, `-vvv` trace; `RUST_LOG` wins when set without -v.
fn init_logging(verbose: u8) {
    use tracing_subscriber::EnvFilter;

    let filter = if verbose == 0 {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"))
    } else {
        let level = match verbose {
            1 => "info",
            2 => "debug",
            _ => "trace",
        };
        // Keep dependency noise one level down from npeer's own logs.
        let directives = match verbose {
            1 => format!("warn,npeer={level},npeer_core={level}"),
            _ => format!("info,npeer={level},npeer_core={level}"),
        };
        EnvFilter::new(directives)
    };

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(verbose >= 2);
    if verbose < 2 {
        subscriber.without_time().init();
    } else {
        subscriber.init();
    }
}
