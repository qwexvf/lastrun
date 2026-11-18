mod daemon;
mod db;
mod history;
mod inventory;
mod report;
mod tui;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::inventory::Source;

/// Record which executables run on this machine and report installed tools that never do.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[arg(long, global = true, default_value = "/var/lib/lastrun/usage.db")]
    db: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Watch every exec on the given filesystems and record it (needs root).
    Daemon {
        /// Seconds between writes to the database.
        #[arg(long, default_value_t = 60)]
        flush: u64,
        /// A path on each filesystem to watch. One mark covers the whole filesystem.
        #[arg(long = "mount", default_values = ["/"])]
        mounts: Vec<PathBuf>,
    },
    /// Backfill usage from a zsh history file (`EXTENDED_HISTORY` format).
    SeedHistory {
        file: PathBuf,
        /// PATH used to resolve command names. sudo resets PATH, so pass "$PATH".
        #[arg(long)]
        path: Option<String>,
    },
    /// List installed tools with no recorded use in the last N days.
    Report {
        #[arg(long, default_value_t = 30)]
        days: u32,
        /// Only this source. Default: all.
        #[arg(long, value_enum)]
        source: Option<Source>,
        /// Also list tools that were used.
        #[arg(long)]
        show_used: bool,
    },
    /// Browse the same data interactively and copy removal commands.
    Tui {
        #[arg(long, default_value_t = 30)]
        days: u32,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Daemon { flush, mounts } => daemon::run(&cli.db, Duration::from_secs(flush), &mounts),
        Cmd::SeedHistory { file, path } => {
            let path = path.or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
            history::seed(&cli.db, &file, &path)
        }
        Cmd::Report { days, source, show_used } => report::run(&cli.db, days, source, show_used),
        Cmd::Tui { days } => tui::run(&cli.db, days),
    }
}
