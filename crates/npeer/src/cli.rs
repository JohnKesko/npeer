//! Command-line interface definition.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::Overrides;
use crate::files::{FileChoice, FileList};
use crate::units;

#[derive(Parser, Debug)]
#[command(
    name = "npeer",
    version,
    about = "A fast, friendly BitTorrent client for the command line",
    long_about = "npeer downloads torrents and magnet links with automatic resume,\n\
                  bandwidth limits and a live progress display.",
    propagate_version = true
)]
pub struct Cli {
    /// Use a specific configuration file
    #[arg(short, long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Increase log detail (-v info, -vv debug, -vvv trace)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Optional so that a bare `npeer` (or `npeer -v` with no command)
    /// shows the help screen instead of a terse error.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Download a torrent file or magnet link
    #[command(visible_alias = "add", visible_alias = "get")]
    Download(DownloadArgs),

    /// Show metadata for a torrent file or magnet link
    Info {
        /// Path to a .torrent file, or a magnet link
        source: String,
        /// List every file, not just the first 20
        #[arg(long)]
        files: bool,
    },

    /// List peers reported by the torrent's trackers
    Peers {
        /// Path to a .torrent file, or a magnet link
        source: String,
    },

    /// List torrents known to npeer
    #[command(visible_alias = "ls")]
    List,

    /// Show detailed status for known torrents
    Status {
        /// Torrent id: an info-hash prefix or part of the name
        id: Option<String>,
    },

    /// Pause a torrent (still picked up by `resume --all`)
    Pause {
        /// Torrent id: an info-hash prefix or part of the name
        id: String,
    },

    /// Resume interrupted or paused downloads
    Resume(ResumeArgs),

    /// Stop a torrent (only resumes when named explicitly)
    Stop {
        /// Torrent id: an info-hash prefix or part of the name
        id: String,
    },

    /// Forget a torrent, optionally deleting its files
    #[command(visible_alias = "rm")]
    Remove {
        /// Torrent id: an info-hash prefix or part of the name
        id: String,
        /// Also delete the downloaded files
        #[arg(long)]
        delete_files: bool,
        /// Skip the confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Re-check downloaded data against the torrent's piece hashes
    Verify {
        /// Torrent id: an info-hash prefix or part of the name
        id: String,
    },

    /// Manage npeer configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Decode a bencoded value and print it as JSON (debugging aid)
    #[command(hide = true)]
    Decode {
        /// Bencoded input; reads stdin when omitted
        value: Option<String>,
    },

    /// Upgrade npeer to the latest released version
    #[cfg(feature = "upgrade")]
    Upgrade {
        /// Only check whether a newer version exists
        #[arg(long)]
        check: bool,
        /// Skip the confirmation question
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },
}

#[derive(Args, Debug)]
pub struct DownloadArgs {
    /// Torrent files or magnet links (several run one after another)
    #[arg(required = true, value_name = "TORRENT|MAGNET")]
    pub sources: Vec<String>,

    #[command(flatten)]
    pub files: FileArgs,

    #[command(flatten)]
    pub tuning: TuningArgs,
}

#[derive(Args, Debug)]
pub struct ResumeArgs {
    /// Torrent id: an info-hash prefix or part of the name.
    /// Run `npeer resume` with no arguments to see what can be resumed.
    #[arg(conflicts_with = "all")]
    pub id: Option<String>,

    /// Resume every unfinished torrent, one after another
    #[arg(long, conflicts_with_all = ["only", "skip"])]
    pub all: bool,

    #[command(flatten)]
    pub files: FileArgs,

    #[command(flatten)]
    pub tuning: TuningArgs,
}

/// Choosing which files of a torrent to download.
#[derive(Args, Debug)]
pub struct FileArgs {
    /// Download only these files, numbered as `npeer info` lists them,
    /// e.g. 1,3,5-8 (`all` downloads every file again)
    #[arg(long, value_name = "FILES", conflicts_with = "skip")]
    pub only: Option<FileList>,

    /// Download every file except these, e.g. 17,18
    #[arg(long, value_name = "FILES")]
    pub skip: Option<FileList>,
}

impl FileArgs {
    pub fn choice(&self) -> Option<FileChoice> {
        match (&self.only, &self.skip) {
            (Some(list), _) => Some(FileChoice::Only(list.clone())),
            (_, Some(list)) => Some(FileChoice::Skip(list.clone())),
            (None, None) => None,
        }
    }
}

/// Flags shared by the commands that run downloads.
#[derive(Args, Debug, Default)]
pub struct TuningArgs {
    /// Save downloads to this directory
    #[arg(short, long, value_name = "DIR")]
    pub output: Option<PathBuf>,

    /// Download bandwidth limit, e.g. 5MiB, 500KiB, unlimited
    #[arg(long, value_name = "RATE")]
    pub limit_down: Option<LimitArg>,

    /// Upload bandwidth limit, e.g. 1MiB, unlimited
    #[arg(long, value_name = "RATE")]
    pub limit_up: Option<LimitArg>,

    /// Port reported to trackers
    #[arg(long, value_name = "PORT")]
    pub port: Option<u16>,

    /// Maximum peer connections
    #[arg(long, value_name = "N")]
    pub max_peers: Option<usize>,

    /// Keep seeding after the download completes
    #[arg(long, conflicts_with = "no_seed")]
    pub seed: bool,

    /// Exit when the download completes, even if seeding is configured
    #[arg(long)]
    pub no_seed: bool,

    /// Re-check data on disk before resuming
    #[arg(long)]
    pub verify: bool,
}

impl TuningArgs {
    pub fn to_overrides(&self) -> Overrides {
        Overrides {
            output: self.output.clone(),
            download_limit: self.limit_down.map(|l| l.0),
            upload_limit: self.limit_up.map(|l| l.0),
            port: self.port,
            max_peers: self.max_peers,
            seed: if self.seed {
                Some(true)
            } else if self.no_seed {
                Some(false)
            } else {
                None
            },
            verify_on_resume: self.verify.then_some(true),
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Write a documented default config file
    Init {
        /// Overwrite an existing config file
        #[arg(long)]
        force: bool,
    },
    /// Print the configuration npeer is running with
    Show,
    /// Print the path of the configuration file
    Path,
    /// Check the configuration file for problems
    Validate,
}

/// A parsed bandwidth limit CLI value; `None` inside means unlimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitArg(pub Option<u64>);

impl std::str::FromStr for LimitArg {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, String> {
        units::parse_limit(input).map(LimitArg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_download_with_tuning() {
        let cli = Cli::parse_from([
            "npeer",
            "download",
            "a.torrent",
            "-o",
            "/tmp/out",
            "--limit-down",
            "5MiB",
            "--no-seed",
        ]);
        let Some(Command::Download(args)) = cli.command else {
            panic!("wrong command")
        };
        assert_eq!(args.sources, vec!["a.torrent"]);
        let overrides = args.tuning.to_overrides();
        assert_eq!(
            overrides.output.as_deref(),
            Some(std::path::Path::new("/tmp/out"))
        );
        assert_eq!(overrides.download_limit, Some(Some(5 * 1024 * 1024)));
        assert_eq!(overrides.seed, Some(false));
    }

    #[test]
    fn limit_unlimited_parses_to_none_override() {
        let cli = Cli::parse_from(["npeer", "download", "x", "--limit-down", "unlimited"]);
        let Some(Command::Download(args)) = cli.command else {
            panic!("wrong command")
        };
        assert_eq!(args.tuning.to_overrides().download_limit, Some(None));
    }

    #[test]
    fn resume_accepts_bare_form_id_or_all() {
        // Bare `resume` lists what can be resumed instead of erroring.
        assert!(Cli::try_parse_from(["npeer", "resume"]).is_ok());
        assert!(Cli::try_parse_from(["npeer", "resume", "--all"]).is_ok());
        assert!(Cli::try_parse_from(["npeer", "resume", "abc"]).is_ok());
        assert!(Cli::try_parse_from(["npeer", "resume", "abc", "--all"]).is_err());
    }

    #[test]
    fn file_choice_flags_parse() {
        let cli = Cli::parse_from(["npeer", "download", "a.torrent", "--skip", "17,18"]);
        let Some(Command::Download(args)) = cli.command else {
            panic!("wrong command")
        };
        assert_eq!(
            args.files.choice(),
            Some(FileChoice::Skip("17,18".parse().unwrap()))
        );

        assert!(Cli::try_parse_from(["npeer", "get", "a", "--only", "1", "--skip", "2"]).is_err());
        assert!(Cli::try_parse_from(["npeer", "download", "a", "--only", "0"]).is_err());
        assert!(Cli::try_parse_from(["npeer", "resume", "abc", "--only", "1-3"]).is_ok());
        assert!(Cli::try_parse_from(["npeer", "resume", "--all", "--skip", "1"]).is_err());
        assert!(Cli::try_parse_from(["npeer", "info", "a.torrent", "--files"]).is_ok());
    }

    #[test]
    fn bare_invocation_parses_without_a_subcommand() {
        // `npeer` and `npeer -v` show help instead of a usage error.
        assert!(Cli::parse_from(["npeer"]).command.is_none());
        assert!(Cli::parse_from(["npeer", "-v"]).command.is_none());
    }

    #[cfg(feature = "upgrade")]
    #[test]
    fn upgrade_flags_parse() {
        let cli = Cli::parse_from(["npeer", "upgrade", "--check"]);
        assert!(matches!(
            cli.command,
            Some(Command::Upgrade {
                check: true,
                yes: false
            })
        ));
        assert!(Cli::try_parse_from(["npeer", "upgrade", "-y"]).is_ok());
    }

    #[test]
    fn verbose_flag_counts() {
        let cli = Cli::parse_from(["npeer", "-vv", "list"]);
        assert_eq!(cli.verbose, 2);
    }
}
