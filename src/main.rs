//! fallow — fast mbox analyzer and slimmer.
//!
//!   fallow stats archive.mbox                      # where does the space go?
//!   fallow slim archive.mbox -o slim.mbox --strip-attachments --drop-bulk
//!
//! The file is memory-mapped and messages are parsed in parallel, so throughput
//! is usually limited by disk speed rather than CPU.

mod mbox;
mod mime;
mod slim;
mod stats;
mod util;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "fallow",
    version,
    about = "Fast mbox analyzer and slimmer: size stats, sender/label filtering, attachment stripping"
)]
struct Cli {
    /// Worker threads (default: all cores)
    #[arg(long, global = true)]
    threads: Option<usize>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Analyze an mbox: totals, attachments, bulk mail, senders, labels; writes CSV reports
    Stats(stats::StatsArgs),
    /// Write a smaller mbox: drop senders/labels/bulk mail, strip or extract attachments
    Slim(slim::SlimArgs),
}

fn main() {
    let cli = Cli::parse();
    if let Some(t) = cli.threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(t.max(1))
            .build_global()
            .ok();
    }
    let res = match cli.cmd {
        Cmd::Stats(a) => stats::run(a),
        Cmd::Slim(a) => slim::run(a),
    };
    if let Err(e) = res {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
