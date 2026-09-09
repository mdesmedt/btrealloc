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
    /// The file or directory to scan.
    path: PathBuf,
    /// Actually perform the extent reallocation operation, writing changes to the filesystem.
    #[arg(long)]
    apply: bool,
    /// Hash every file before and after it is rewritten. This is an optional validation as the kernel already checks chunk equality.
    #[arg(long)]
    verify: bool,
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
        verify: args.verify,
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

    // Scan phase

    println!("Scanning: {}", options.path.display());
    let scan = match btrealloc::scan(&options) {
        Ok(scan) => scan,
        Err(e) => {
            eprintln!("{}: {e}", options.path.display());
            return ExitCode::FAILURE;
        }
    };
    scan.report();

    if !options.dryrun && !options.apply {
        println!();
        println!(
            "Nothing done. Specify --dryrun to see what would be done, or --apply to rewrite extents."
        );
        return ExitCode::SUCCESS;
    }

    // Run phase

    let report = btrealloc::run(&options, &scan);
    if report.corrupted.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
