//! End to end tests against real btrfs filesystems. Every one of them needs
//! root: the search ioctl btrealloc reads extents through does, and so do mkfs
//! and the loop mount each test builds its filesystem with.
//!
//! They are left out of `cargo test` for that reason. To run them:
//!
//!     ./runvm.sh                      in a throwaway VM, as `nix flake check` does
//!     cargo test --test vm --no-run   then run the binary it names under sudo
//!
//! Each test builds only the shapes it needs, on a filesystem of its own, and
//! asserts on what the phases return rather than on what they printed.
//!
//! Most of them mount plainly. Running every one of them against each
//! combination of `autodefrag` and `compress=zstd:3` was tried and dropped: it
//! quadrupled the suite and never once told two runs apart, because the shapes
//! here are built from `write_random`, which is incompressible by design and so
//! allocates plain extents whatever the mount says. The two places where the
//! mount can still matter say so themselves — `compressed_extents_are_counted_on_disk`
//! asks for compression, and `random_layouts_release_every_extent` picks a
//! combination from its seed.
//!
//! How a write is cut into extents is btrfs's to decide, not ours to assume, so
//! the assertions below are written against the layout the scan actually found
//! wherever they can be. Where a shape only means something in one extent, the
//! test says so through [`support::single_extent`], and an unexpected layout
//! fails as a fixture problem rather than as a btrealloc bug.

mod support;

use support::{Fs, MIB};

/// The size of the files these shapes are built from. btrfs caps one extent at
/// 128 MiB, and a write which sits exactly on that cap is the one most likely
/// to come back split in two, so this keeps well under it.
const FILE_MIB: u64 = 64;

/// One extent with a live piece at each end. The waste is between them, and a
/// rewrite copies the two ends to get it back.
#[test]
fn a_bookend_extent_is_reclaimed() {
    let fs = Fs::new();
    let data = fs.dir("data");
    let file = fs.path("data/bookend");
    support::bookend(&file, FILE_MIB);

    let before = support::checksums(&data);
    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    let reclaimable = support::file_reclaimable(&scan, &file);
    assert!(
        reclaimable >= 16 * MIB,
        "the hole should leave most of the extent unreachable, found {reclaimable} bytes"
    );
    assert!(
        support::job_for(&worklist, &file).is_some(),
        "the file should be worth rewriting"
    );

    let (_, report) = support::apply(&data);
    assert!(report.corrupted.is_empty(), "{:?}", report.corrupted);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert!(report.freed_bytes >= reclaimable);

    let after = support::scan(&data);
    let (allocated, used) = support::file_totals(&after, &file);
    assert_eq!(allocated, used, "nothing should be wasted afterwards");
    assert_eq!(before, support::checksums(&data), "contents changed");
}

/// Three live pieces of one extent, so the rewrite has three ranges to copy and
/// three ranges to point back at the copy.
#[test]
fn three_live_pieces_are_copied() {
    let fs = Fs::new();
    let data = fs.dir("data");
    let file = fs.path("data/pieces");
    support::write_random(&file, FILE_MIB);
    support::punch_hole(&file, MIB, 19 * MIB); // keeps [0, 1)
    support::punch_hole(&file, 30 * MIB, 26 * MIB); // keeps [20, 30) and [56, 64)
    let live = (1 + 10 + 8) * MIB;

    let before = support::checksums(&data);
    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    let chunks: Vec<u64> = worklist
        .iter()
        .flat_map(|extent| &extent.refs)
        .filter(|r| **r.path == *file)
        .map(|r| r.num_bytes)
        .collect();
    // Three surviving pieces are at least three chunks: a split extent divides
    // them further, it never merges two of them into one.
    assert!(
        chunks.len() >= 3,
        "the three surviving pieces should be three chunks or more, found {chunks:?}"
    );
    assert_eq!(
        chunks.iter().sum::<u64>(),
        live,
        "the job should copy exactly what is left of the file"
    );

    let (_, report) = support::apply(&data);
    assert!(report.corrupted.is_empty(), "{:?}", report.corrupted);

    let after = support::scan(&data);
    let (allocated, used) = support::file_totals(&after, &file);
    assert_eq!(allocated, used);
    assert_eq!(before, support::checksums(&data), "contents changed");
}

/// One extent, two files, each keeping a different end of it. Neither file
/// alone can free it, so the job carries both and both are rewritten.
#[test]
fn one_extent_held_by_two_files_needs_both_rewritten() {
    let fs = Fs::new();
    let data = fs.dir("data");
    let (a, b) = (fs.path("data/multi-a"), fs.path("data/multi-b"));
    support::write_random_one_extent(&a, FILE_MIB);
    support::reflink(&a, &b);
    support::punch_hole(&a, MIB, (FILE_MIB - 1) * MIB);
    support::punch_hole(&b, 0, (FILE_MIB - 1) * MIB);

    // The two ends only belong to one extent if the write came back as one:
    // split, each file would hold an extent of its own and there would be
    // nothing here to test.
    let extent = support::single_extent(&a);
    assert_eq!(
        support::single_extent(&b),
        extent,
        "fixture: the two files should hold one extent between them"
    );

    let before = support::checksums(&data);
    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    let job = support::job_for(&worklist, &a).expect("the shared extent is worth rewriting");
    assert_eq!(job.disk_address, extent);
    assert_eq!(job.holders().len(), 2, "both files hold the extent");

    let (_, report) = support::apply(&data);
    assert!(report.corrupted.is_empty(), "{:?}", report.corrupted);

    let after = support::scan(&data);
    for file in [&a, &b] {
        let (allocated, used) = support::file_totals(&after, file);
        assert_eq!(allocated, used, "{} still wastes space", file.display());
    }
    assert_eq!(before, support::checksums(&data), "contents changed");
}

/// Two files over the same bytes of one extent. The rewrite copies those bytes
/// once and points both at the one copy: a private copy each would cost more
/// than the extent it drops returns.
#[test]
fn identical_holders_still_share_one_copy() {
    let fs = Fs::new();
    let data = fs.dir("data");
    let (a, b) = (fs.path("data/same-a"), fs.path("data/same-b"));
    support::write_random(&a, FILE_MIB);
    support::reflink(&a, &b);
    support::punch_hole(&a, MIB, (FILE_MIB - 1) * MIB);
    support::punch_hole(&b, MIB, (FILE_MIB - 1) * MIB);

    let before = support::checksums(&data);
    let scan = support::scan(&data);
    let allocated_before = scan.totals().allocated_bytes;

    let (_, report) = support::apply(&data);
    assert!(report.corrupted.is_empty(), "{:?}", report.corrupted);

    assert_eq!(
        support::physical_extents(&a),
        support::physical_extents(&b),
        "the two files no longer share one copy"
    );
    let after = support::scan(&data);
    assert!(
        after.totals().allocated_bytes < allocated_before,
        "the rewrite should have given space back, not taken more"
    );
    assert_eq!(before, support::checksums(&data), "contents changed");
}

/// An extent something outside the scanned path also holds. Rather than
/// guessing at what that other reference means, the scan resolves it like any
/// other: this tool works on the whole filesystem, the scanned path just
/// decides where it starts looking. Punching the same hole on both sides
/// leaves the extent genuinely part dead, so it lands on the worklist and both
/// holders — the one outside `data` included — end up rewritten.
#[test]
fn an_extent_referenced_outside_the_scan_is_reclaimed_too() {
    let fs = Fs::new();
    let data = fs.dir("data");
    fs.dir("outside");
    let keeper = fs.path("outside/keeper");
    let external = fs.path("data/external");
    support::write_random(&keeper, FILE_MIB);
    support::reflink(&keeper, &external);
    support::punch_hole(&keeper, MIB, (FILE_MIB - 2) * MIB);
    support::punch_hole(&external, MIB, (FILE_MIB - 2) * MIB);

    let before = support::checksums(fs.root()); // covers both `data` and `outside`
    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    assert_eq!(
        scan.totals().unreachable_bytes,
        support::file_reclaimable(&scan, &external),
        "the hole both files share should show up as reclaimable"
    );
    assert!(
        worklist.iter().any(|extent| extent.holders().len() == 2),
        "the extent is on the worklist with both its holders"
    );

    let (_, report) = support::apply(&data);
    assert!(report.corrupted.is_empty(), "{:?}", report.corrupted);

    assert_eq!(
        support::physical_extents(&keeper),
        support::physical_extents(&external),
        "the two files should still share one copy"
    );
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// nodatacow files are overwritten in place and cannot be deduped, so however
/// much of their extents is dead, they are not ours to rewrite.
#[test]
fn a_nodatacow_file_never_reaches_the_worklist() {
    let fs = Fs::new();
    let data = fs.dir("data");
    let file = fs.path("data/nocow");
    std::fs::File::create(&file).expect("create the file");
    support::set_nocow(&file);
    support::write_random(&file, FILE_MIB);
    support::punch_hole(&file, MIB, (FILE_MIB - 2) * MIB);

    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    assert!(
        scan.extents
            .values()
            .all(|extent| extent.refs.iter().all(|r| r.nocow)),
        "the scan should see the file as nodatacow"
    );
    assert!(worklist.is_empty(), "so it is never worked on");
}

/// A datacow file in a nodatacow directory. The temporary copy inherits the
/// directory's flag, and btrfs will not dedupe between inodes that disagree
/// about checksums, so the job is reached and then skipped by name.
#[test]
fn a_nodatacow_directory_is_skipped_by_name() {
    let fs = Fs::new();
    let data = fs.dir("data");
    let dir = fs.dir("data/latecow");
    let file = fs.path("data/latecow/datacow");
    support::bookend(&file, FILE_MIB);
    // +C after the file exists: it keeps its checksums, anything made
    // alongside it later does not.
    support::set_nocow(&dir);

    let before = support::checksums(&data);
    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    assert!(
        support::job_for(&worklist, &file).is_some(),
        "the waste is real, so the job is made"
    );

    let (_, report) = support::apply(&data);
    assert!(report.corrupted.is_empty(), "{:?}", report.corrupted);
    assert!(report.rewritten.is_empty(), "nothing could be rewritten");
    let (path, reason) = report.skipped.first().expect("the job was skipped");
    assert_eq!(path, &file, "the file it was charged to");
    assert!(
        reason.contains("nodatacow"),
        "the reason should name the cause, said {reason:?}"
    );

    assert_eq!(before, support::checksums(&data), "contents changed");
    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    assert!(
        support::job_for(&worklist, &file).is_some(),
        "it is still on the worklist, and always will be"
    );
}

/// Waste too small to be worth an operation: reported in the totals, never
/// worked on.
#[test]
fn waste_below_the_floor_is_reported_but_not_worked() {
    let fs = Fs::new();
    let data = fs.dir("data");
    let file = fs.path("data/tiny");
    support::write_random(&file, 1);
    support::punch_hole(&file, 64 * 1024, 8 * 1024);

    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    assert!(
        support::file_reclaimable(&scan, &file) > 0,
        "the hole is dead space and is reported"
    );
    assert!(worklist.is_empty(), "but it is not worth an operation");
}

/// A big extent with a little dead space: the copy costs far more than it
/// returns, so the ratio gate keeps us off it.
#[test]
fn an_extent_too_full_to_be_worth_copying_is_left_alone() {
    let fs = Fs::new();
    let data = fs.dir("data");
    let file = fs.path("data/full");
    support::write_random(&file, FILE_MIB);
    support::punch_hole(&file, (FILE_MIB / 2) * MIB, 8 * MIB);

    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    let extents = support::extents_of(&scan, &file);
    // Whatever the write was cut into, an extent which would cost more than
    // four bytes moved for every byte it returns is a bad trade, and the ratio
    // gate has to keep us off it.
    let full: Vec<_> = extents
        .iter()
        .filter(|(_, extent)| extent.live_uncompressed_bytes() > 4 * extent.disk_free_bytes())
        .collect();
    assert!(
        !full.is_empty(),
        "fixture: the hole should have left an extent which is mostly live, found {:?}",
        extents
            .iter()
            .map(|(address, extent)| (
                address,
                extent.live_uncompressed_bytes(),
                extent.disk_free_bytes()
            ))
            .collect::<Vec<_>>(),
    );
    for (address, extent) in full {
        assert!(
            !support::on_worklist(&worklist, *address),
            "extent {address:#x} would copy {} bytes to free {}, which is not worth doing",
            extent.live_uncompressed_bytes(),
            extent.disk_free_bytes(),
        );
    }
}

/// A file with no holes wastes nothing, and must never turn up as work.
#[test]
fn a_clean_file_has_no_waste() {
    let fs = Fs::new();
    let data = fs.dir("data");
    let file = fs.path("data/clean");
    support::write_random(&file, 8);

    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    assert_eq!(support::file_reclaimable(&scan, &file), 0);
    assert!(worklist.is_empty());
}

/// The temporary copy is made in each file's own directory, so a run has to
/// reach into every directory it works in, not one fixed place.
#[test]
fn files_are_rewritten_in_their_own_directories() {
    let fs = Fs::new();
    let data = fs.dir("data");
    fs.dir("data/sub/deeper");
    let shallow = fs.path("data/sub/nested");
    let deep = fs.path("data/sub/deeper/nested");
    support::bookend(&shallow, FILE_MIB);
    support::bookend(&deep, FILE_MIB);

    let before = support::checksums(&data);
    let (_, report) = support::apply(&data);
    assert!(report.corrupted.is_empty(), "{:?}", report.corrupted);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&data);
    for file in [&shallow, &deep] {
        let (allocated, used) = support::file_totals(&after, file);
        assert_eq!(allocated, used, "{} was not rewritten", file.display());
    }
    assert_eq!(before, support::checksums(&data), "contents changed");
}

/// The copy is an O_TMPFILE, which never makes a directory entry, so nothing
/// can be stranded: not by a rewrite that worked, and not by one that failed.
#[test]
fn no_temporary_file_is_left_behind() {
    let fs = Fs::new();
    let data = fs.dir("data");
    let dir = fs.dir("data/latecow");
    support::bookend(&fs.path("data/bookend"), FILE_MIB);
    support::bookend(&fs.path("data/latecow/datacow"), FILE_MIB);
    support::set_nocow(&dir);

    let before = support::listing(fs.root());
    let (_, report) = support::apply(&data);
    // Both paths have to be walked for this to prove anything: one rewrite
    // which worked, and one which made a temporary and then failed.
    assert!(!report.rewritten.is_empty(), "no rewrite worked");
    assert!(!report.skipped.is_empty(), "no rewrite failed");

    assert_eq!(
        before,
        support::listing(fs.root()),
        "the filesystem gained or lost entries"
    );
}

/// A dry run walks the same worklist in the same order and writes nothing.
#[test]
fn a_dry_run_changes_nothing() {
    let fs = Fs::new();
    let data = fs.dir("data");
    let file = fs.path("data/bookend");
    support::bookend(&file, FILE_MIB);

    let before = support::checksums(&data);
    let listing = support::listing(fs.root());
    let extents = support::physical_extents(&file);

    let (_, report) = support::dryrun(&data);
    assert!(!report.rewritten.is_empty(), "it should have found work");
    assert!(report.skipped.is_empty());
    assert!(report.corrupted.is_empty());

    assert_eq!(before, support::checksums(&data), "contents changed");
    assert_eq!(listing, support::listing(fs.root()), "the listing changed");
    assert_eq!(
        extents,
        support::physical_extents(&file),
        "the file's extents moved"
    );
}

/// On a compressed filesystem an extent's on-disk size is not its uncompressed
/// size, and what a reference uses has to be counted in on-disk bytes.
#[test]
fn compressed_extents_are_counted_on_disk() {
    let fs = Fs::with_options(2048, "compress=zstd:3");
    let data = fs.dir("data");
    let file = fs.path("data/compressed");
    support::write_compressible(&file, 4);
    // Inside one compressed extent, which btrfs caps at 128 KiB uncompressed,
    // so this leaves a partly-referenced extent rather than dropping a whole one.
    support::punch_hole(&file, 16 * 1024, 32 * 1024);

    let scan = support::scan(&data);
    let (allocated, used) = support::file_totals(&scan, &file);
    assert!(
        allocated < 4 * MIB,
        "the data should have compressed, {allocated} bytes on disk"
    );
    assert!(used > 0 && used < allocated, "used {used} of {allocated}");
    assert!(
        scan.totals().unreachable_bytes > 0,
        "the hole leaves unreachable on-disk bytes"
    );
}

/// The shape a block-level deduplicator leaves on a real filesystem: one large
/// extent nothing holds any more except a single sector in each of several small
/// files, each sector a different part of the extent.
/// An extent frees only when the last of them goes, so more than one holder is
/// the whole point of the shape.
#[test]
fn every_holder_of_a_slivered_extent_is_redirected() {
    const HOLDERS: u64 = 4;
    const SMALL_MIB: u64 = 1;

    let fs = Fs::new();
    let data = fs.dir("data");

    // One extent, then a sector of it handed to each small file. The big file
    // goes afterwards, leaving the extent held only by the slivers.
    let big = fs.path("data/big");
    support::write_random_one_extent(&big, FILE_MIB);
    let address = support::single_extent(&big);

    let holders: Vec<_> = (0..HOLDERS)
        .map(|i| {
            let path = fs.path(&format!("data/small{i}"));
            // A different sector of the extent each time, so the rewrite has one
            // range per holder to copy rather than one shared between them.
            support::sliver(&big, (i + 1) * MIB, &path, SMALL_MIB * MIB, i * 4096);
            path
        })
        .collect();
    std::fs::remove_file(&big).expect("remove the big file");
    support::sync_fs();

    let before = support::checksums(&data);
    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    let extent = scan
        .extents
        .get(&address)
        .expect("the extent should survive");
    assert_eq!(
        extent.refs.len(),
        HOLDERS as usize,
        "every sliver should be found as a reference",
    );
    assert!(
        support::on_worklist(&worklist, address),
        "an extent held by slivers alone is almost entirely waste",
    );

    let (_, report) = support::apply(&data);
    assert!(report.corrupted.is_empty(), "{:?}", report.corrupted);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    // The point of the test: not that the rewrite reported success, but that
    // every holder actually moved and the extent is gone.
    let after = support::scan(&data);
    assert!(
        !after.extents.contains_key(&address),
        "the extent is still there, so some holder was never redirected",
    );
    for holder in &holders {
        assert!(
            !support::physical_extents(holder).contains(&address),
            "{} still references the extent",
            holder.display(),
        );
    }
    assert_eq!(before, support::checksums(&data), "contents changed");
}

/// The same shape at the size it actually occurs: btrfs caps one extent at
/// 128 MiB, and the extents left slivered on a filesystem a deduplicator has
/// been over are that cap almost every time. The slivers are spread the width
/// of the extent, so the copy the rewrite makes is scattered across a temporary
/// as long as the extent itself.
#[test]
fn every_holder_of_a_full_size_slivered_extent_is_redirected() {
    const HOLDERS: u64 = 8;

    let fs = Fs::new();
    let sectorsize = fs.sectorsize();
    let data = fs.dir("data");

    // 128 MiB is btrfs's cap, and a write that sits on it can come back split,
    // so take whichever extent it gave us the most of and slice that one.
    let big = fs.path("data/big");
    support::write_random_one_extent(&big, 128);
    let biggest = btrealloc::kernel::file_extents(&big)
        .expect("read the big file's extents")
        .into_iter()
        .max_by_key(|extent| extent.disk_bytes)
        .expect("the big file should hold an extent");
    let address = biggest.disk_address;
    assert!(
        biggest.disk_bytes >= 64 * MIB,
        "fixture: wanted a large extent, btrfs gave {} bytes",
        biggest.disk_bytes,
    );

    // One sector per holder, spread the width of the extent, each in a directory
    // of its own the way a deduplicator finds them.
    let step = (biggest.disk_bytes / HOLDERS) & !(sectorsize - 1);
    let holders: Vec<_> = (0..HOLDERS)
        .map(|i| {
            let dir = fs.dir(&format!("data/d{i}"));
            let path = dir.join("photo.jpg");
            support::sliver(&big, biggest.file_offset + i * step, &path, MIB, 8192);
            path
        })
        .collect();
    std::fs::remove_file(&big).expect("remove the big file");
    support::sync_fs();

    let before = support::checksums(&data);
    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    let extent = scan
        .extents
        .get(&address)
        .expect("the extent should survive");
    assert_eq!(
        extent.refs.len(),
        HOLDERS as usize,
        "every sliver should be found as a reference",
    );
    assert!(support::on_worklist(&worklist, address), "almost all waste");

    let (_, report) = support::apply(&data);
    assert!(report.corrupted.is_empty(), "{:?}", report.corrupted);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&data);
    let left: Vec<_> = holders
        .iter()
        .filter(|holder| support::physical_extents(holder).contains(&address))
        .collect();
    assert!(
        left.is_empty(),
        "{} of {HOLDERS} holders still reference the extent: {left:?}",
        left.len(),
    );
    assert!(
        !after.extents.contains_key(&address),
        "the extent is still there",
    );
    assert_eq!(before, support::checksums(&data), "contents changed");
}

/// The topology a real run walks, rather than one job on its own: several
/// slivered extents, and holders which hold a sector of each of them.
/// Every job here rewrites files a later job rewrites again.
#[test]
fn holders_shared_between_jobs_all_move() {
    const EXTENTS: u64 = 3;
    const HOLDERS: u64 = 4;

    let fs = Fs::new();
    let sectorsize = fs.sectorsize();
    let data = fs.dir("data");

    // One large extent per round, each sliced by every holder, so each holder
    // ends up referencing a sector of all of them.
    let mut addresses = Vec::new();
    let holders: Vec<_> = (0..HOLDERS)
        .map(|h| fs.path(&format!("data/photo{h}.jpg")))
        .collect();
    for e in 0..EXTENTS {
        let big = fs.path(&format!("data/big{e}"));
        support::write_random_one_extent(&big, 128);
        let biggest = btrealloc::kernel::file_extents(&big)
            .expect("read the big file's extents")
            .into_iter()
            .max_by_key(|extent| extent.disk_bytes)
            .expect("the big file should hold an extent");
        assert!(biggest.disk_bytes >= 64 * MIB, "fixture: extent too small");
        addresses.push(biggest.disk_address);

        let step = (biggest.disk_bytes / HOLDERS) & !(sectorsize - 1);
        for (h, path) in holders.iter().enumerate() {
            let src = biggest.file_offset + h as u64 * step;
            // A sector of this extent at a place in the holder no other round
            // has used, so each holder accumulates one reference per extent.
            let dest_offset = (e + 1) * 64 * 1024 + h as u64 * sectorsize;
            if e == 0 {
                support::sliver(&big, src, path, MIB, dest_offset);
            } else {
                support::sliver_into(&big, src, path, dest_offset);
            }
        }
        std::fs::remove_file(&big).expect("remove the big file");
    }
    support::sync_fs();

    let before = support::checksums(&data);
    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    for address in &addresses {
        let extent = scan
            .extents
            .get(address)
            .expect("the extent should survive");
        assert_eq!(extent.refs.len(), HOLDERS as usize, "every sliver found");
        assert!(
            support::on_worklist(&worklist, *address),
            "almost all waste"
        );
    }

    let (_, report) = support::apply(&data);
    assert!(report.corrupted.is_empty(), "{:?}", report.corrupted);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&data);
    for address in &addresses {
        let left: Vec<_> = holders
            .iter()
            .filter(|holder| support::physical_extents(holder).contains(address))
            .collect();
        assert!(left.is_empty(), "{address:#x} still held by {left:?}");
        assert!(
            !after.extents.contains_key(address),
            "{address:#x} still there"
        );
    }
    assert_eq!(before, support::checksums(&data), "contents changed");
}

/// A large file keeping one sector of an extent it used to own outright, plus a
/// small file holding a sector of the same extent. The two disagree about how
/// file offsets map onto extent offsets, which a deleted big file never shows.
#[test]
fn a_holder_deep_inside_a_large_file_moves_too() {
    let fs = Fs::with_options(4096, "");
    let sectorsize = fs.sectorsize();
    let data = fs.dir("data");

    // Big enough that btrfs gives it more than one extent, so the one we work
    // on starts well into the file and file offset and extent offset diverge.
    let movie = fs.path("data/movie.mov");
    support::write_random(&movie, 200);
    // Not the first extent: the point of the shape is a holder whose file
    // offset is nowhere near its offset inside the extent.
    let biggest = btrealloc::kernel::file_extents(&movie)
        .expect("read the movie's extents")
        .into_iter()
        .filter(|extent| extent.file_offset > 0)
        .max_by_key(|extent| extent.disk_bytes)
        .expect("the movie should hold an extent past its start");
    let address = biggest.disk_address;
    let (base, size) = (biggest.file_offset, biggest.disk_bytes);
    assert!(size >= 64 * MIB, "fixture: extent too small ({size} bytes)");
    assert!(
        base > 0,
        "fixture: wanted an extent past the start of the file"
    );

    // A deduplicator points one sector of a small file at a sector near the front
    // of that extent, while the movie will keep one near the back.
    let kept = (size / 2) & !(sectorsize - 1);
    let shared = (size / 8) & !(sectorsize - 1);
    let photo = fs.path("data/photo.jpg");
    support::sliver(&movie, base + shared, &photo, MIB, 8192);

    // Now drop everything the movie still held of that extent except one sector,
    // leaving it holding a sliver of an extent it used to own outright.
    support::punch_hole(&movie, base, kept);
    support::punch_hole(&movie, base + kept + sectorsize, size - kept - sectorsize);

    let before = support::checksums(&data);
    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    let extent = scan
        .extents
        .get(&address)
        .expect("the extent should survive");
    assert_eq!(extent.refs.len(), 2, "the movie and the photo hold it");
    assert!(support::on_worklist(&worklist, address), "almost all waste");

    let (_, report) = support::apply(&data);
    assert!(report.corrupted.is_empty(), "{:?}", report.corrupted);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&data);
    for holder in [&movie, &photo] {
        assert!(
            !support::physical_extents(holder).contains(&address),
            "{} still references the extent",
            holder.display(),
        );
    }
    assert!(
        !after.extents.contains_key(&address),
        "the extent is still there"
    );
    assert_eq!(before, support::checksums(&data), "contents changed");
}

/// Holders at the offsets inside the extent that they really have.
/// Not evenly spaced from the start, which is not what a deduplicator leaves.
#[test]
fn holders_at_arbitrary_extent_offsets_all_move() {
    // Taken from one extent of a filesystem this went wrong on.
    const OFFSETS: [u64; 7] = [
        7557120, 8368128, 66936832, 71802880, 76017664, 87293952, 91471872,
    ];

    let fs = Fs::with_options(4096, "");
    let sectorsize = fs.sectorsize();
    let data = fs.dir("data");

    let big = fs.path("data/big");
    support::write_random_one_extent(&big, 128);
    let biggest = btrealloc::kernel::file_extents(&big)
        .expect("read the big file's extents")
        .into_iter()
        .max_by_key(|extent| extent.disk_bytes)
        .expect("the big file should hold an extent");
    let address = biggest.disk_address;
    assert!(
        biggest.disk_bytes > *OFFSETS.last().unwrap() + sectorsize,
        "fixture: extent is {} bytes, too small for these offsets",
        biggest.disk_bytes,
    );

    let holders: Vec<_> = OFFSETS
        .iter()
        .enumerate()
        .map(|(i, &offset)| {
            let path = fs.path(&format!("data/photo{i}.jpg"));
            // Somewhere different in each holder too, so file offset and extent
            // offset have nothing to do with each other.
            support::sliver(
                &big,
                biggest.file_offset + offset,
                &path,
                8 * MIB,
                i as u64 * 65536 + 4096,
            );
            path
        })
        .collect();
    std::fs::remove_file(&big).expect("remove the big file");
    support::sync_fs();

    let before = support::checksums(&data);
    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    let extent = scan
        .extents
        .get(&address)
        .expect("the extent should survive");
    assert_eq!(extent.refs.len(), OFFSETS.len(), "every sliver found");
    assert!(support::on_worklist(&worklist, address), "almost all waste");

    let (_, report) = support::apply(&data);
    assert!(report.corrupted.is_empty(), "{:?}", report.corrupted);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&data);
    let left: Vec<_> = holders
        .iter()
        .filter(|h| support::physical_extents(h).contains(&address))
        .collect();
    assert!(
        left.is_empty(),
        "{} holders still hold it: {left:?}",
        left.len()
    );
    assert!(
        !after.extents.contains_key(&address),
        "the extent is still there"
    );
    assert_eq!(before, support::checksums(&data), "contents changed");
}

/// Two holders whose references into the extent begin together and end apart.
/// A stretch is as long as the first reference that asked for it, so the shorter
/// one is a part of that stretch rather than the whole of it.
#[test]
fn a_holder_whose_reference_is_shorter_than_the_stretch_moves() {
    let fs = Fs::new();
    let sectorsize = fs.sectorsize();
    let data = fs.dir("data");

    let big = fs.path("data/big");
    support::write_random_one_extent(&big, FILE_MIB);
    let address = support::single_extent(&big);

    // "long" is reached first in path order, so its two sectors are what the
    // stretch is cut to; "short" then holds only the first of them.
    let long = fs.path("data/long.jpg");
    let short = fs.path("data/short.jpg");
    support::sliver_of(&big, 8 * MIB, &long, MIB, 8192, 2 * sectorsize);
    support::sliver_of(&big, 8 * MIB, &short, MIB, 8192, sectorsize);
    std::fs::remove_file(&big).expect("remove the big file");
    support::sync_fs();

    let before = support::checksums(&data);
    let scan = support::scan(&data);
    let extent = scan
        .extents
        .get(&address)
        .expect("the extent should survive");
    assert_eq!(extent.refs.len(), 2, "both holders should be found");

    let (_, report) = support::apply(&data);
    assert!(report.corrupted.is_empty(), "{:?}", report.corrupted);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&data);
    assert!(
        !after.extents.contains_key(&address),
        "the extent is still there"
    );
    assert_eq!(before, support::checksums(&data), "contents changed");
}

/// A live stretch tens of MiB long, held through a single reference: all of it
/// has to be copied and redirected or the holder keeps the extent alive.
///
/// Nothing else in the suite keeps a stretch this large, so a rewrite that
/// mishandles a long staged copy shows up here.
#[test]
fn a_long_live_stretch_moves_whole() {
    let fs = Fs::with_options(4096, "");
    let data = fs.dir("data");

    let movie = fs.path("data/movie.mov");
    support::write_random_one_extent(&movie, 200);
    let biggest = btrealloc::kernel::file_extents(&movie)
        .expect("read the movie's extents")
        .into_iter()
        .max_by_key(|extent| extent.disk_bytes)
        .expect("the movie should hold an extent");
    let address = biggest.disk_address;
    let (base, size) = (biggest.file_offset, biggest.disk_bytes);
    assert!(
        size >= 100 * MIB,
        "fixture: extent too small ({size} bytes)"
    );

    // Kept: 40 MiB starting 5 MiB in, so the stretch is neither a whole number
    // of dedupes long nor aligned to one from the start of the extent.
    const LIVE: u64 = 40 * MIB;
    let start = 5 * MIB;
    support::punch_hole(&movie, base, start);
    support::punch_hole(&movie, base + start + LIVE, size - start - LIVE);

    let before = support::checksums(&data);
    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    let extent = scan
        .extents
        .get(&address)
        .expect("the extent should survive");
    assert_eq!(
        extent.live_uncompressed_bytes(),
        LIVE,
        "the middle stretch should be all that is left"
    );
    assert!(
        support::on_worklist(&worklist, address),
        "most of it is waste"
    );

    let (_, report) = support::apply(&data);
    assert!(report.corrupted.is_empty(), "{:?}", report.corrupted);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&data);
    assert!(
        !support::physical_extents(&movie).contains(&address),
        "the movie still references the extent",
    );
    assert!(
        !after.extents.contains_key(&address),
        "the extent is still there"
    );
    assert_eq!(before, support::checksums(&data), "contents changed");
}

/// The same long stretch, with a deduplicator's sliver sitting one sector deep
/// inside it. That sector is held by a file which holds nothing else, so a
/// rewrite that only redirects the holder it started from leaves the sliver's
/// file pointing at the extent.
#[test]
fn a_holder_deep_inside_a_long_stretch_moves() {
    let fs = Fs::with_options(4096, "");
    let data = fs.dir("data");

    let movie = fs.path("data/movie.mov");
    support::write_random_one_extent(&movie, 200);
    let biggest = btrealloc::kernel::file_extents(&movie)
        .expect("read the movie's extents")
        .into_iter()
        .max_by_key(|extent| extent.disk_bytes)
        .expect("the movie should hold an extent");
    let address = biggest.disk_address;
    let (base, size) = (biggest.file_offset, biggest.disk_bytes);
    assert!(
        size >= 100 * MIB,
        "fixture: extent too small ({size} bytes)"
    );

    const LIVE: u64 = 40 * MIB;
    let start = 5 * MIB;
    // Well inside the stretch, far from either end.
    let inside = start + 16 * MIB;
    let photo = fs.path("data/photo.jpg");
    support::sliver(&movie, base + inside, &photo, MIB, 8192);

    support::punch_hole(&movie, base, start);
    support::punch_hole(&movie, base + start + LIVE, size - start - LIVE);

    let before = support::checksums(&data);
    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    let extent = scan
        .extents
        .get(&address)
        .expect("the extent should survive");
    assert_eq!(extent.refs.len(), 2, "the movie and the photo hold it");
    assert_eq!(
        extent.live_uncompressed_bytes(),
        LIVE,
        "the photo's sector is inside the stretch"
    );
    assert!(
        support::on_worklist(&worklist, address),
        "most of it is waste"
    );

    let (_, report) = support::apply(&data);
    assert!(report.corrupted.is_empty(), "{:?}", report.corrupted);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&data);
    for holder in [&movie, &photo] {
        assert!(
            !support::physical_extents(holder).contains(&address),
            "{} still references the extent",
            holder.display(),
        );
    }
    assert!(
        !after.extents.contains_key(&address),
        "the extent is still there"
    );
    assert_eq!(before, support::checksums(&data), "contents changed");
}

/// A file whose size is not a whole number of sectors. Its last reference runs
/// past the end of its data, so the stretch to copy is longer than there is
/// anything to read, and the dedupe which redirects it ends mid-sector.
#[test]
fn a_holder_with_a_short_last_block_moves() {
    let fs = Fs::new();
    let data = fs.dir("data");

    let file = fs.path("data/ragged");
    support::write_random(&file, FILE_MIB);
    // Off a sector boundary, so the last file extent covers more than the file
    // holds.
    let size = FILE_MIB * MIB - 1234;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&file)
        .expect("open the file to truncate")
        .set_len(size)
        .expect("truncate the file");
    support::sync_fs();
    // Bookended, so the extent is mostly waste and the surviving tail is the
    // ragged one.
    support::punch_hole(&file, MIB, (FILE_MIB - 4) * MIB);

    let before = support::checksums(&data);
    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    assert!(
        support::job_for(&worklist, &file).is_some(),
        "the file should be worth rewriting"
    );

    let (_, report) = support::apply(&data);
    assert!(report.corrupted.is_empty(), "{:?}", report.corrupted);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&data);
    let (allocated, used) = support::file_totals(&after, &file);
    assert_eq!(allocated, used, "nothing should be wasted afterwards");
    assert_eq!(before, support::checksums(&data), "contents changed");
}

/// A layout nobody designed: files, holes, reflinks, deduplicator slivers and
/// ragged truncations, at random offsets in a random order, then the whole run
/// over whatever came out.
///
/// The hand-written shapes above each test one thing we already thought of.
/// This one tests the thing we did not: every extent the run reports freed has
/// to be one no holder points at any more, and no file may come back holding
/// different bytes.
///
/// `BTREALLOC_FUZZ_SEED` and `BTREALLOC_FUZZ_ITERS` widen or replay a run without a
/// rebuild. The seed of each layout is printed before it is built, so a failure
/// names the filesystem that caused it.
#[test]
fn random_layouts_release_every_extent() {
    fn env(name: &str, fallback: u64) -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(fallback)
    }

    let base = env("BTREALLOC_FUZZ_SEED", 0x9E37_79B9_7F4A_7C15);
    let iters = env("BTREALLOC_FUZZ_ITERS", 8);

    for i in 0..iters {
        let seed = base.wrapping_add(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        println!("fuzz seed {seed:#x}");
        let mut rng = support::Rng::new(seed);

        // The seed picks the mount too. Compression changes what an extent's
        // on-disk size means and autodefrag rewrites extents of its own accord,
        // and this is the one test whose data is sometimes compressible, so it
        // is the one place where varying them is worth the filesystems it costs.
        let fs = Fs::with_options(
            2048,
            match seed % 4 {
                0 => "",
                1 => "autodefrag",
                2 => "compress=zstd:3",
                _ => "autodefrag,compress=zstd:3",
            },
        );
        let data = fs.dir("data");
        support::random_layout(&data, &mut rng);

        let before = support::checksums(&data);
        // apply() re-reads every holder of every extent the report calls freed,
        // and fails if one still points at it.
        let (_, report) = support::apply(&data);
        assert!(
            report.corrupted.is_empty(),
            "seed {seed:#x}: {:?}",
            report.corrupted
        );
        assert!(
            report.skipped.is_empty(),
            "seed {seed:#x}: {:?}",
            report.skipped
        );
        assert_eq!(
            before,
            support::checksums(&data),
            "seed {seed:#x}: contents changed"
        );
    }
}

/// Two files over one extent, the second a byte or two short of a whole number
/// of sectors. Its last reference is a partial sector, and the copy it has to be
/// pointed at is longer than it is, so the dedupe which moves that sector asks
/// for a sub-sector length from the middle of the copy.
///
/// The kernel reports those bytes deduped and leaves the sector where it was, so
/// the short file keeps its reference and the extent never frees, while the run
/// counts the job done.
#[test]
fn a_holder_ending_mid_block_lets_go() {
    let fs = Fs::new();
    let sectorsize = fs.sectorsize();
    let data = fs.dir("data");
    let (long, short) = (fs.path("data/long"), fs.path("data/short"));
    support::write_random_one_extent(&long, 8);
    support::reflink(&long, &short);

    // A size which is not a whole number of sectors, and shorter than the file
    // the copy will be filled from.
    let ragged = 8 * MIB - 1234;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&short)
        .expect("open the short file")
        .set_len(ragged)
        .expect("truncate the short file");
    support::sync_fs();

    // Waste in the middle, so the extent is worth rewriting and the tail of
    // each file is what survives at the far end of it.
    support::punch_hole(&long, MIB, 5 * MIB);
    support::punch_hole(&short, MIB, 5 * MIB);

    let extent = support::single_extent(&long);
    assert_eq!(
        support::single_extent(&short),
        extent,
        "fixture: the two files should hold one extent between them"
    );
    assert_eq!(
        ragged % sectorsize,
        sectorsize - 1234,
        "fixture: the tail is a part sector"
    );

    let before = support::checksums(&data);
    let scan = support::scan(&data);
    let worklist = support::worklist(&scan);
    assert!(
        support::on_worklist(&worklist, extent),
        "the hole is most of it"
    );

    let (_, report) = support::apply(&data);
    assert!(report.corrupted.is_empty(), "{:?}", report.corrupted);

    let after = support::scan(&data);
    for file in [&long, &short] {
        assert!(
            !support::physical_extents(file).contains(&extent),
            "{} still references the extent",
            file.display(),
        );
    }
    assert!(
        !after.extents.contains_key(&extent),
        "the extent is still there"
    );
    assert_eq!(before, support::checksums(&data), "contents changed");
}
