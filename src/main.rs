use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use btrealloc::Options;

unsafe extern "C" {
    fn geteuid() -> u32;
}

#[derive(Parser)]
#[command(about = "Reclaim unreachable space on btrfs by rewriting live data into compact extents")]
struct Args {
    /// Where the filesystem's top-level subvolume is mounted (`mount -o subvolid=5`).
    /// Every subvolume and snapshot on it is scanned from there.
    #[arg(value_name = "MOUNT")]
    path: PathBuf,
    /// Actually perform the extent reallocation operation, writing changes to the filesystem.
    #[arg(long)]
    apply: bool,
    /// Increase logging verbosity.
    #[arg(short, long)]
    verbose: bool,
    /// Perform a dry run without writing any data.
    #[arg(long)]
    dryrun: bool,
}

fn parse_options() -> Options {
    let args = Args::parse();
    if args.apply && args.dryrun {
        eprintln!("Cannot specify both --apply and --dryrun");
        std::process::exit(1);
    }
    Options {
        apply: args.apply,
        dryrun: args.dryrun,
        verbose: args.verbose,
        path: args.path,
    }
}

fn main() -> ExitCode {
    let options = parse_options();

    // Warning
    println!(
        "WARNING: This tool is experimental and modifies your filesystem as root. Use at your own risk."
    );

    // btrfs tree-search and dedupe ioctls are root-only; without them the tool
    // silently does nothing useful, so refuse to start.
    if unsafe { geteuid() } != 0 {
        eprintln!("Error: btrealloc must be run as root, it relies on btrfs ioctls");
        return ExitCode::FAILURE;
    }

    // Scan, and rewrite along the way if asked to

    println!("Scanning: {}", options.path.display());
    let (stats, _) = match btrealloc::run(&options) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("{}: {e}", options.path.display());
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
