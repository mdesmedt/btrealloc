use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use log::{Level, LevelFilter, Metadata, Record};

use btrealloc::Options;

unsafe extern "C" {
    fn geteuid() -> u32;
}

#[derive(Parser)]
#[command(
    about = "Reclaim unreachable space on btrfs by rewriting live data into compact extents",
    after_help = "Exit status:\n  \
        0  the run finished, even if some extents were left alone\n  \
        1  the run could not finish\n  \
        2  invalid arguments"
)]
struct Args {
    /// Where the filesystem's top-level subvolume is mounted (`mount -o subvolid=5`).
    /// Every subvolume and snapshot on it is scanned from there.
    #[arg(value_name = "MOUNT")]
    path: PathBuf,
    /// Actually perform the extent reallocation operation, writing changes to the filesystem.
    #[arg(long, conflicts_with = "dryrun")]
    apply: bool,
    /// Also say why each extent was left alone, and list every file holding
    /// each rewritten extent.
    #[arg(short, long)]
    verbose: bool,
    /// Perform a dry run without writing any data.
    #[arg(long)]
    dryrun: bool,
    /// Threads to resolve extents on. Defaults to one per CPU.
    #[arg(short, long, value_name = "N")]
    jobs: Option<NonZeroUsize>,
}

/// Writes btrealloc's diagnostics to stderr, at the level `-v` asks
/// for: errors and warnings named as such, more detail as it is. What a run
/// finds and does is its result, printed to stdout, and does not come through
/// here.
struct Logger;

impl log::Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level() && metadata.target().starts_with("btrealloc")
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        match record.level() {
            Level::Error => eprintln!("error: {}", record.args()),
            Level::Warn => eprintln!("warning: {}", record.args()),
            Level::Info | Level::Debug | Level::Trace => eprintln!("{}", record.args()),
        }
    }

    fn flush(&self) {}
}

static LOGGER: Logger = Logger;

fn parse_options() -> Options {
    let args = Args::parse();
    let level = if args.verbose {
        LevelFilter::Info
    } else {
        LevelFilter::Warn
    };
    log::set_logger(&LOGGER).expect("the logger is set once, before anything logs");
    log::set_max_level(level);
    // Extents are resolved on rayon's global pool, which is one thread per CPU
    // unless told otherwise here.
    if let Some(jobs) = args.jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(jobs.get())
            .build_global()
            .expect("the global thread pool is set up once, before anything uses it");
    }
    Options {
        apply: args.apply,
        dryrun: args.dryrun,
        path: args.path,
    }
}

fn main() -> ExitCode {
    let options = parse_options();

    // btrfs tree-search and dedupe ioctls are root-only; without them the tool
    // silently does nothing useful, so refuse to start.
    if unsafe { geteuid() } != 0 {
        log::error!("btrealloc must be run as root, it relies on btrfs ioctls");
        return ExitCode::FAILURE;
    }

    // Scan, and rewrite along the way if asked to

    println!("Scanning: {}", options.path.display());
    let (stats, _) = match btrealloc::run(&options) {
        Ok(result) => result,
        Err(e) => {
            log::error!("{}: {e}", options.path.display());
            return ExitCode::FAILURE;
        }
    };
    // The totals are counted as the walk finds each extent, before it is
    // rewritten, so after --apply they describe the filesystem as it was.
    if options.apply {
        stats.report_left_alone();
    } else {
        println!();
        stats.report();
    }

    if !options.dryrun && !options.apply {
        println!();
        println!(
            "Nothing done. Specify --dryrun to see what would be done, or --apply to rewrite extents."
        );
    }

    ExitCode::SUCCESS
}
