use std::io::{self, IsTerminal};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;

use clap::Parser;

use btrealloc::Options;
use btrealloc::console::Console;
use btrealloc::extent::WorthChecks;

unsafe extern "C" {
    fn geteuid() -> u32;
}

#[derive(Parser)]
#[command(about = "Reclaim unreachable space on btrfs by rewriting live data into compact extents")]
struct Args {
    /// Where the filesystem's top-level subvolume is mounted (`mount -o subvolid=5`),
    /// or with --whole-filesystem where any other subvolume is. Every subvolume and
    /// snapshot on the filesystem is scanned from there.
    #[arg(value_name = "MOUNT")]
    path: PathBuf,
    /// Scan the whole filesystem through MOUNT when it is a subvolume other than the top-level one,
    /// as on a system that only mounts its subvolumes. Every other subvolume and snapshot is scanned,
    /// and reallocated when --apply is passed.
    #[arg(long)]
    whole_filesystem: bool,
    /// Actually perform the extent reallocation operation, writing changes to the filesystem.
    #[arg(long)]
    apply: bool,
    /// Prints information about each extent during the scan or apply.
    #[arg(short, long)]
    verbose: bool,
    /// Threads to resolve extents on. Defaults to one per CPU.
    #[arg(short, long, value_name = "N")]
    jobs: Option<NonZeroUsize>,
    /// Also rewrite extents that free under 64 KiB each. Reclaims more, at the
    /// cost of more small rewrites.
    #[arg(long)]
    no_min_free: bool,
    /// Also rewrite extents that copy over 4x the space they free. Reclaims
    /// more, at the cost of copying more data.
    #[arg(long)]
    no_copy_limit: bool,
}

fn parse_options() -> (Options, Console) {
    let args = Args::parse();
    // Extents are resolved on rayon's global pool, which is one thread per CPU
    // unless told otherwise here.
    if let Some(jobs) = args.jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(jobs.get())
            .build_global()
            .expect("the global thread pool is set up once, before anything uses it");
    }
    let options = Options {
        apply: args.apply,
        path: args.path,
        whole_filesystem: args.whole_filesystem,
        worth_checks: WorthChecks {
            min_free: !args.no_min_free,
            copy_ratio: !args.no_copy_limit,
        },
    };
    let interactive = io::stderr().is_terminal();
    (options, Console::new(args.verbose, interactive))
}

fn main() -> ExitCode {
    let (options, console) = parse_options();
    let console = Rc::new(console);

    // btrfs tree-search and dedupe ioctls are root-only; without them the tool
    // silently does nothing useful, so refuse to start.
    if unsafe { geteuid() } != 0 {
        console.error("btrealloc must be run as root, it relies on btrfs ioctls");
        return ExitCode::FAILURE;
    }

    // Scan, and rewrite along the way if asked to

    console.print(format!("Scanning: {}", options.path.display()));
    let (stats, report) = match btrealloc::run(&options, &console) {
        Ok(result) => result,
        Err(e) => {
            console.error(format!("{}: {e}", options.path.display()));
            return ExitCode::FAILURE;
        }
    };
    // The totals are counted as the walk finds each extent, before it is
    // rewritten, so after --apply they describe the filesystem as it was.
    if options.apply {
        stats.report_left_alone(&console);
    } else {
        console.print("");
        stats.report(&report, &console);
        console.print("");
        console.print("Nothing written. Run with --apply to reallocate extents.");
    }

    ExitCode::SUCCESS
}
