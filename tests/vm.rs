//! End to end tests against real btrfs filesystems. Every one of them needs
//! root: the search ioctl btrealloc reads extents through does, and so do mkfs
//! and the loop mount each test builds its filesystem with.
//!
//! They are left out of `cargo test` for that reason. To run them:
//!
//!     ./runvm.sh                      in a throwaway VM, as `nix flake check` does
//!     cargo test --test vm --no-run   then run the binary it names under sudo
//!
//! Each test builds only the shapes it needs, on a filesystem of its own, runs
//! the tool over all of it from its top-level subvolume, as the tool requires,
//! and asserts on what the phases return rather than on what they printed.
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

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use btrealloc::extent::LeftAlone;
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
    fs.dir("data");
    let file = fs.path("data/bookend");
    support::bookend(&file, FILE_MIB);

    let before = support::checksums(fs.root());
    let scan = support::scan(&fs);
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

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert!(report.freed_bytes >= reclaimable);

    let after = support::scan(&fs);
    let (allocated, used) = support::file_totals(&after, &file);
    assert_eq!(allocated, used, "nothing should be wasted afterwards");
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// Three live pieces of one extent, so the rewrite has three ranges to copy and
/// three ranges to point back at the copy.
#[test]
fn three_live_pieces_are_copied() {
    let fs = Fs::new();
    fs.dir("data");
    let file = fs.path("data/pieces");
    support::write_random(&file, FILE_MIB);
    support::punch_hole(&file, MIB, 19 * MIB); // keeps [0, 1)
    support::punch_hole(&file, 30 * MIB, 26 * MIB); // keeps [20, 30) and [56, 64)
    let live = (1 + 10 + 8) * MIB;

    let before = support::checksums(fs.root());
    let scan = support::scan(&fs);
    let worklist = support::worklist(&scan);
    let id = support::identity(&file);
    let chunks: Vec<u64> = worklist
        .iter()
        .flat_map(|extent| &extent.refs)
        .filter(|r| (r.root, r.inode) == id)
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

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);

    let after = support::scan(&fs);
    let (allocated, used) = support::file_totals(&after, &file);
    assert_eq!(allocated, used);
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// One extent, two files, each keeping a different end of it. Neither file
/// alone can free it, so the job carries both and both are rewritten.
#[test]
fn one_extent_held_by_two_files_needs_both_rewritten() {
    let fs = Fs::new();
    fs.dir("data");
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

    let before = support::checksums(fs.root());
    let scan = support::scan(&fs);
    let worklist = support::worklist(&scan);
    let job = support::job_for(&worklist, &a).expect("the shared extent is worth rewriting");
    assert_eq!(job.disk_address, extent);
    assert_eq!(job.holders().len(), 2, "both files hold the extent");

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);

    let after = support::scan(&fs);
    for file in [&a, &b] {
        let (allocated, used) = support::file_totals(&after, file);
        assert_eq!(allocated, used, "{} still wastes space", file.display());
    }
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// Extents the extent tree records against the tree block holding their
/// references rather than against the files, as snapshot history leaves them.
/// The extent tree cannot say who holds those, so the scan asks the kernel's
/// backreference walk. A file holding an extent alone and two files sharing
/// one must come back whole and be rewritten.
#[test]
fn extents_behind_shared_backreferences_are_reclaimed() {
    let fs = Fs::new();
    fs.subvolume("original");
    fs.filler("original/filler");
    fs.dir("original/data");
    // One reference in all: only the first MiB is left.
    let alone = fs.path("original/data/alone");
    support::write_random_one_extent(&alone, FILE_MIB);
    support::punch_hole(&alone, MIB, (FILE_MIB - 1) * MIB);
    // Four references, two files each keeping both ends.
    let (a, b) = (
        fs.path("original/data/shared-a"),
        fs.path("original/data/shared-b"),
    );
    support::write_random_one_extent(&a, FILE_MIB);
    support::reflink(&a, &b);
    support::punch_hole(&a, MIB, (FILE_MIB - 2) * MIB);
    support::punch_hole(&b, MIB, (FILE_MIB - 2) * MIB);

    fs.snapshot_and_delete("original", "snapshot");
    let alone = fs.path("snapshot/data/alone");
    let (a, b) = (
        fs.path("snapshot/data/shared-a"),
        fs.path("snapshot/data/shared-b"),
    );
    assert!(
        fs.shared_data_backrefs() > 0,
        "fixture: the snapshot should be left with shared data backreferences"
    );

    let before = support::checksums(fs.root());
    let scan = support::scan(&fs);
    let worklist = support::worklist(&scan);
    assert!(
        support::job_for(&worklist, &alone).is_some(),
        "the file holding its extent alone is worth rewriting"
    );
    let job = support::job_for(&worklist, &a).expect("the shared extent is worth rewriting");
    assert_eq!(job.holders().len(), 2, "both files hold the extent");
    assert_eq!(job.refs.len(), 4, "each file holds both ends of it");

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&fs);
    for file in [&alone, &a, &b] {
        let (allocated, used) = support::file_totals(&after, file);
        assert_eq!(allocated, used, "{} still wastes space", file.display());
    }
    assert_eq!(
        support::physical_extents(&a),
        support::physical_extents(&b),
        "the two files should still share one copy"
    );
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

// Extents held from more than one subvolume.
//
// The tool runs over the whole filesystem, so an extent a snapshot or another
// subvolume also holds is rewritten in every one of them, read-only snapshots
// included, and freed like any other.
//
// How the extent tree records that a snapshot holds an extent depends on
// whether the leaf holding the file's extent items has been written to since
// the snapshot, and each shape takes its own path through the resolution. One
// test for each.

/// A file of one extent with only its first and last MiB left: worth rewriting
/// wherever it is found alone.
fn wasteful_file(path: &Path) {
    support::write_random_one_extent(path, FILE_MIB);
    support::punch_hole(path, MIB, (FILE_MIB - 2) * MIB);
}

/// Runs over the whole filesystem, and checks that the extent at `extent`,
/// which every one of `files` holds, is found with all of them, rewritten, and
/// gone from each, and that they still share one copy of what they held.
fn assert_reclaimed_from_all(fs: &Fs, files: &[PathBuf], extent: u64) {
    let before = support::checksums(fs.root());
    let scan = support::scan(fs);
    for reason in [LeftAlone::Changed, LeftAlone::Unresolved] {
        assert_eq!(
            scan.stats.left_alone.get(reason).count,
            0,
            "the scan left extents alone: {}",
            reason.label()
        );
    }
    let found = scan
        .extents
        .get(&extent)
        .expect("the scan should find the extent");
    assert_eq!(
        found.holders().len(),
        files.len(),
        "the scan should find every file holding the extent"
    );
    assert!(
        support::on_worklist(&support::worklist(&scan), extent),
        "the extent is worth rewriting"
    );

    let (_, report) = support::apply(fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert!(
        report.rewritten.contains(&extent),
        "the extent was not rewritten"
    );
    for file in files {
        assert!(
            !support::physical_extents(file).contains(&extent),
            "{} still references the extent",
            file.display()
        );
    }
    let copy = support::physical_extents(&files[0]);
    for file in &files[1..] {
        assert_eq!(
            support::physical_extents(file),
            copy,
            "{} no longer shares one copy with {}",
            file.display(),
            files[0].display()
        );
    }
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// The snapshot shares the leaf holding the file's extent items, and neither
/// side has written to it since. The extent tree records one reference, from
/// the live subvolume, with nothing to say a second tree reaches it.
///
/// Data written after the snapshot is the live subvolume's alone, and is
/// reclaimed alongside it.
#[test]
fn an_extent_a_snapshot_reaches_through_an_untouched_leaf_is_reclaimed_from_both() {
    // Without noatime, reading a file for its checksum writes its inode, and
    // with it the leaf this test needs untouched.
    let fs = Fs::with_options(2048, "noatime");
    fs.subvolume("live");
    fs.dir("live/old");
    let old = fs.path("live/old/file");
    wasteful_file(&old);
    fs.filler("live/filler");
    // Made before the snapshot, after the filler: what goes in it later lands
    // in leaves at the far end of the tree from the old file's.
    fs.dir("live/new");
    fs.snapshot("live", "snapshot");

    let new = fs.path("live/new/file");
    wasteful_file(&new);
    support::sync_fs();

    let snapshotted = fs.path("snapshot/old/file");
    let extent = support::single_extent(&old);
    assert_eq!(
        support::single_extent(&snapshotted),
        extent,
        "fixture: the snapshot should hold the old file's extent"
    );
    assert_eq!(
        fs.shared_data_backrefs(),
        0,
        "fixture: no leaf should have been written since the snapshot"
    );
    let new_extent = support::single_extent(&new);

    assert_reclaimed_from_all(&fs, &[old, snapshotted], extent);
    assert!(
        !support::physical_extents(&new).contains(&new_extent),
        "data written since the snapshot was not rewritten"
    );
    assert!(
        fs.is_readonly("snapshot"),
        "the snapshot is no longer read-only"
    );
}

/// The live subvolume has written to the leaf holding the file's extent items
/// since the snapshot, which gave it a copy of its own. The snapshot's is left
/// reaching the extent through a shared data backreference, which only the
/// kernel's backreference walk can follow.
#[test]
fn an_extent_a_snapshot_reaches_through_a_changed_leaf_is_reclaimed_from_both() {
    let fs = Fs::new();
    fs.subvolume("live");
    fs.dir("live/data");
    let file = fs.path("live/data/file");
    wasteful_file(&file);
    fs.filler("live/filler");
    fs.snapshot("live", "snapshot");

    // Writing the file's inode writes the leaf it shares with its extent items.
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
        .expect("change the file's mode");
    support::sync_fs();

    let snapshotted = fs.path("snapshot/data/file");
    let extent = support::single_extent(&file);
    assert_eq!(
        support::single_extent(&snapshotted),
        extent,
        "fixture: the snapshot should hold the file's extent"
    );
    assert!(
        fs.shared_data_backrefs() > 0,
        "fixture: the snapshot should be left with shared data backreferences"
    );

    assert_reclaimed_from_all(&fs, &[file, snapshotted], extent);
    assert!(
        fs.is_readonly("snapshot"),
        "the snapshot is no longer read-only"
    );
}

/// The subvolume holds the extent through a shared data backreference already,
/// its only one, as snapshot history leaves them (see
/// `extents_behind_shared_backreferences_are_reclaimed`). A snapshot taken
/// since reaches it through that same leaf, which leaves the extent tree
/// exactly as it was: one reference, naming the leaf rather than a tree.
#[test]
fn an_extent_behind_a_shared_backreference_a_snapshot_also_reaches_is_reclaimed_from_both() {
    let fs = Fs::new();
    fs.subvolume("original");
    fs.filler("original/filler");
    fs.dir("original/data");
    // Only the first MiB left, so the file holds the extent through one
    // reference: the count the extent tree gives is then one too.
    let original = fs.path("original/data/file");
    support::write_random_one_extent(&original, FILE_MIB);
    support::punch_hole(&original, MIB, (FILE_MIB - 1) * MIB);
    fs.snapshot_and_delete("original", "live");
    let shared = fs.shared_data_backrefs();
    assert!(
        shared > 0,
        "fixture: the history should leave shared data backreferences"
    );

    fs.snapshot("live", "snapshot");
    let file = fs.path("live/data/file");
    let snapshotted = fs.path("snapshot/data/file");
    let extent = support::single_extent(&file);
    assert_eq!(
        support::single_extent(&snapshotted),
        extent,
        "fixture: the snapshot should hold the file's extent"
    );
    assert_eq!(
        fs.shared_data_backrefs(),
        shared,
        "fixture: the snapshot should not have changed the extent tree"
    );

    assert_reclaimed_from_all(&fs, &[file, snapshotted], extent);
    assert!(
        fs.is_readonly("snapshot"),
        "the snapshot is no longer read-only"
    );
}

/// A read-only snapshot whose subvolume has since been deleted, so it is the
/// only thing left holding the extent. Nothing can be written inside it, so the
/// copy has to be made elsewhere for the rewrite to happen at all.
#[test]
fn a_read_only_snapshot_as_the_only_holder_is_reclaimed() {
    let fs = Fs::new();
    fs.subvolume("original");
    fs.dir("original/data");
    wasteful_file(&fs.path("original/data/file"));
    fs.snapshot("original", "snapshot");
    fs.delete_subvolume("original");

    let file = fs.path("snapshot/data/file");
    let extent = support::single_extent(&file);
    assert_reclaimed_from_all(&fs, &[file], extent);
    assert!(
        fs.is_readonly("snapshot"),
        "the snapshot is no longer read-only"
    );
}

/// Several read-only snapshots of one subvolume, one of them taken after the
/// live file's leaf was written to, so the extent is reached both through a
/// leaf the extent tree names and through ones only the backreference walk can
/// follow. Every one of them has to let go for the extent to be freed.
#[test]
fn an_extent_held_by_many_read_only_snapshots_is_reclaimed() {
    let fs = Fs::new();
    fs.subvolume("live");
    fs.dir("live/data");
    let file = fs.path("live/data/file");
    wasteful_file(&file);
    fs.filler("live/filler");
    fs.snapshot("live", "first");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
        .expect("change the file's mode");
    support::sync_fs();
    fs.snapshot("live", "second");
    fs.snapshot("live", "third");

    let extent = support::single_extent(&file);
    let mut files = vec![file];
    for snapshot in ["first", "second", "third"] {
        let path = fs.path(&format!("{snapshot}/data/file"));
        assert_eq!(
            support::single_extent(&path),
            extent,
            "fixture: {snapshot} should hold the file's extent"
        );
        files.push(path);
    }

    assert_reclaimed_from_all(&fs, &files, extent);
    for snapshot in ["first", "second", "third"] {
        assert!(
            fs.is_readonly(snapshot),
            "{snapshot} is no longer read-only"
        );
    }
}

/// One extent held by more snapshots than the process may have files open.
/// Holding a descriptor for every subvolume looked up would run out of them
/// before the last holder was found.
///
/// The limit is lowered for the test first. At the usual 1024 the snapshots it
/// takes to exceed it make this the slowest test in the suite by far, most of
/// it spent checksumming them.
#[test]
fn an_extent_held_by_more_snapshots_than_open_files_allow_is_reclaimed() {
    let lowered = support::OpenFilesLimit::lower_to(64);
    let limit = lowered.soft();

    let fs = Fs::new();
    fs.subvolume("live");
    let file = fs.path("live/file");
    wasteful_file(&file);

    let mut files = vec![file];
    for i in 0..limit + 16 {
        let name = format!("snap{i}");
        fs.snapshot("live", &name);
        files.push(fs.path(&format!("{name}/file")));
    }
    let extent = support::single_extent(&files[0]);
    assert_eq!(
        support::single_extent(files.last().unwrap()),
        extent,
        "fixture: the last snapshot should hold the file's extent"
    );

    assert_reclaimed_from_all(&fs, &files, extent);
}

/// One extent held from two subvolumes side by side, through a reflink between
/// them.
#[test]
fn an_extent_reflinked_across_subvolumes_is_reclaimed() {
    let fs = Fs::new();
    fs.subvolume("a");
    fs.subvolume("b");
    let (first, second) = (fs.path("a/file"), fs.path("b/file"));
    wasteful_file(&first);
    support::reflink(&first, &second);

    let extent = support::single_extent(&first);
    assert_reclaimed_from_all(&fs, &[first, second], extent);
}

/// A subvolume inside a directory of another subvolume, so the path to a file
/// in it is only found by working up through both.
#[test]
fn nested_subvolumes_are_reached() {
    let fs = Fs::new();
    fs.subvolume("outer");
    fs.dir("outer/dir");
    fs.subvolume("outer/dir/inner");
    fs.dir("outer/dir/inner/deeper");
    let file = fs.path("outer/dir/inner/deeper/file");
    let copy = fs.path("outer/copy");
    wasteful_file(&file);
    support::reflink(&file, &copy);

    let extent = support::single_extent(&file);
    assert_reclaimed_from_all(&fs, &[file, copy], extent);
}

/// Two extents next to each other on disk, read from the extent tree together,
/// both in files whose leaf a snapshot shares untouched. Rewriting the first
/// writes that leaf, which changes who the extent tree says holds the second,
/// but only once the transaction commits: acted on as first read, the
/// snapshot's hold on the second would be missed and it would never be freed.
#[test]
fn an_extent_whose_holders_change_mid_walk_is_still_released() {
    // Without noatime, reading the files for their checksums writes the leaf
    // before the rewrite gets to.
    let fs = Fs::with_options(2048, "noatime");
    fs.subvolume("live");
    fs.dir("live/old");
    let (a, b) = (fs.path("live/old/a"), fs.path("live/old/b"));
    wasteful_file(&a);
    wasteful_file(&b);
    fs.filler("live/filler");
    fs.snapshot("live", "snapshot");
    assert_eq!(
        fs.shared_data_backrefs(),
        0,
        "fixture: no leaf should have been written since the snapshot"
    );

    let files = [
        a.clone(),
        b.clone(),
        fs.path("snapshot/old/a"),
        fs.path("snapshot/old/b"),
    ];
    let extents = [support::single_extent(&a), support::single_extent(&b)];

    let before = support::checksums(fs.root());
    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    for extent in extents {
        assert!(
            report.rewritten.contains(&extent),
            "{extent:#x} was not rewritten"
        );
        for file in &files {
            assert!(
                !support::physical_extents(file).contains(&extent),
                "{} still references {extent:#x}",
                file.display()
            );
        }
    }
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// A rewrite allocates new extents, often further along the disk than the walk
/// has got. Those are fully used from the start, and the walk has to pass over
/// them rather than count, or rewrite, the same data twice.
#[test]
fn extents_rewritten_mid_walk_are_not_revisited() {
    let fs = Fs::new();
    fs.dir("data");
    for i in 0..4 {
        support::bookend(&fs.path(&format!("data/bookend{i}")), FILE_MIB);
    }

    let before = support::scan(&fs);
    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert!(!report.rewritten.is_empty(), "nothing was rewritten");
    for address in &report.rewritten {
        assert!(
            before.extents.contains_key(address),
            "rewrote {address:#x}, which the run made itself"
        );
    }
    let mut once = report.rewritten.clone();
    once.sort_unstable();
    once.dedup();
    assert_eq!(
        once.len(),
        report.rewritten.len(),
        "an extent was rewritten twice"
    );

    let (_, again) = support::dryrun(&fs);
    assert!(
        again.rewritten.is_empty(),
        "a second run still found work: {:x?}",
        again.rewritten
    );
}

/// Anything other than where the top-level subvolume is mounted is refused
/// before anything is read, with what to do instead. So is rewriting through a
/// read-only mount of it, though reading through one is fine.
#[test]
fn only_the_top_level_subvolume_is_accepted() {
    let fs = Fs::new();
    fs.dir("data");
    let file = fs.path("data/bookend");
    support::bookend(&file, FILE_MIB);
    fs.subvolume("live");
    let inside = fs.dir("live/dir");
    let mounted = fs.mount_subvolume("live");
    let readonly = fs.mount_readonly();

    let before = support::checksums(fs.root());
    let extents = support::physical_extents(&file);

    let refused: [(PathBuf, &str); 6] = [
        (fs.path("data"), "inside the top-level subvolume"),
        (file.clone(), "not a directory"),
        (fs.path("live"), "subvolid=5"),
        (inside, "subvolid=5"),
        (mounted.path.clone(), "subvolid=5"),
        (fs.tmpfs(), "not on a btrfs filesystem"),
    ];
    for (path, expected) in &refused {
        for apply in [false, true] {
            let error = btrealloc::run(&support::options(path, apply, false))
                .err()
                .unwrap_or_else(|| panic!("{} was accepted", path.display()));
            assert!(
                error.to_string().contains(expected),
                "{}: said {error:?}, which should mention {expected:?}",
                path.display()
            );
        }
    }

    let error = btrealloc::run(&support::options(&readonly.path, true, false))
        .err()
        .expect("--apply through a read-only mount was accepted");
    assert!(
        error.to_string().contains("read-only"),
        "said {error:?}, which should mention the mount is read-only"
    );
    let (_, report) = btrealloc::run(&support::options(&readonly.path, false, true))
        .expect("a dry run through a read-only mount");
    assert!(!report.rewritten.is_empty(), "the dry run should find work");

    assert_eq!(before, support::checksums(fs.root()), "contents changed");
    assert_eq!(
        extents,
        support::physical_extents(&file),
        "the file's extents moved"
    );
}

/// A read-only mount over part of the filesystem, as NixOS puts over
/// `/nix/store`, does not keep the files under it from being rewritten: the
/// tool works through a copy of the mount it was given, which nothing is
/// mounted on.
#[test]
fn a_readonly_mount_on_top_does_not_get_in_the_way() {
    let fs = Fs::new();
    fs.dir("store");
    let file = fs.path("store/bookend");
    support::bookend(&file, FILE_MIB);
    let _over = fs.mount_readonly_over("store");

    let before = support::checksums(fs.root());
    let extents = support::physical_extents(&file);

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert!(!report.rewritten.is_empty(), "the file should be rewritten");

    assert_eq!(before, support::checksums(fs.root()), "contents changed");
    assert_ne!(
        extents,
        support::physical_extents(&file),
        "the file's extents should have moved"
    );
}

/// Two files over the same bytes of one extent. The rewrite copies those bytes
/// once and points both at the one copy: a private copy each would cost more
/// than the extent it drops returns.
#[test]
fn identical_holders_still_share_one_copy() {
    let fs = Fs::new();
    fs.dir("data");
    let (a, b) = (fs.path("data/same-a"), fs.path("data/same-b"));
    support::write_random(&a, FILE_MIB);
    support::reflink(&a, &b);
    support::punch_hole(&a, MIB, (FILE_MIB - 1) * MIB);
    support::punch_hole(&b, MIB, (FILE_MIB - 1) * MIB);

    let before = support::checksums(fs.root());
    let scan = support::scan(&fs);
    let allocated_before = scan.totals().allocated_bytes;

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);

    assert_eq!(
        support::physical_extents(&a),
        support::physical_extents(&b),
        "the two files no longer share one copy"
    );
    let after = support::scan(&fs);
    assert!(
        after.totals().allocated_bytes < allocated_before,
        "the rewrite should have given space back, not taken more"
    );
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// nodatacow files are overwritten in place and cannot be deduped, so however
/// much of their extents is dead, they are not ours to rewrite. The extent tree
/// does not say which files are nodatacow, so the extent is found and judged
/// worth it, and only the run, opening the file, finds out.
#[test]
fn a_nodatacow_file_is_never_rewritten() {
    let fs = Fs::new();
    fs.dir("data");
    let file = fs.path("data/nocow");
    std::fs::File::create(&file).expect("create the file");
    support::set_nocow(&file);
    support::write_random(&file, FILE_MIB);
    support::punch_hole(&file, MIB, (FILE_MIB - 2) * MIB);
    let extents = support::physical_extents(&file);

    let scan = support::scan(&fs);
    assert!(
        support::job_for(&support::worklist(&scan), &file).is_some(),
        "fixture: the waste should be worth rewriting, were the file not nodatacow"
    );

    let (_, report) = support::apply(&fs);
    assert!(report.rewritten.is_empty(), "{:x?}", report.rewritten);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert_eq!(
        extents,
        support::physical_extents(&file),
        "the file's extents moved"
    );
}

/// The run skips a nodatacow file for being nodatacow, not for some other
/// reason that happens to leave it alone too.
#[test]
fn a_nodatacow_file_is_skipped_as_nodatacow() {
    let fs = Fs::new();
    fs.dir("data");
    let file = fs.path("data/nocow");
    std::fs::File::create(&file).expect("create the file");
    support::set_nocow(&file);
    support::write_random(&file, FILE_MIB);
    support::punch_hole(&file, MIB, (FILE_MIB - 2) * MIB);

    let options = support::options(fs.root(), true, false);
    let (stats, report) = btrealloc::run(&options).expect("run over the fixture");
    assert!(report.rewritten.is_empty(), "{:x?}", report.rewritten);
    assert!(
        stats.left_alone.get(LeftAlone::NoDataCow).count > 0,
        "the file was not skipped for being nodatacow"
    );
}

/// A datacow file in a nodatacow directory, on a filesystem whose top
/// directory, where the temporary copies are made, is nodatacow too. A copy
/// inherits the flag, and btrfs will not dedupe between inodes that disagree
/// about checksums, so the copy has to shed it for the rewrite to work.
#[test]
fn a_nodatacow_directory_does_not_stop_a_rewrite() {
    let fs = Fs::new();
    let dir = fs.dir("data/latecow");
    let file = fs.path("data/latecow/datacow");
    support::bookend(&file, FILE_MIB);
    // +C after the file exists: it keeps its checksums, anything made
    // alongside it later does not.
    support::set_nocow(&dir);
    support::set_nocow(fs.root());

    let before = support::checksums(fs.root());
    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert!(!report.rewritten.is_empty(), "nothing was rewritten");

    let after = support::scan(&fs);
    let (allocated, used) = support::file_totals(&after, &file);
    assert_eq!(allocated, used, "nothing should be wasted afterwards");
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// Waste too small to be worth an operation: reported in the totals, never
/// worked on.
#[test]
fn waste_below_the_floor_is_reported_but_not_worked() {
    let fs = Fs::new();
    fs.dir("data");
    let file = fs.path("data/tiny");
    support::write_random(&file, 1);
    support::punch_hole(&file, 64 * 1024, 8 * 1024);

    let scan = support::scan(&fs);
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
    fs.dir("data");
    let file = fs.path("data/full");
    support::write_random(&file, FILE_MIB);
    support::punch_hole(&file, (FILE_MIB / 2) * MIB, 8 * MIB);

    let scan = support::scan(&fs);
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
    fs.dir("data");
    let file = fs.path("data/clean");
    support::write_random(&file, 8);

    let scan = support::scan(&fs);
    let worklist = support::worklist(&scan);
    assert_eq!(support::file_reclaimable(&scan, &file), 0);
    assert!(worklist.is_empty());
}

/// Files deep in a directory tree are found by their inode and reached by the
/// path the kernel gives for it.
#[test]
fn files_in_nested_directories_are_rewritten() {
    let fs = Fs::new();
    fs.dir("data/sub/deeper");
    let shallow = fs.path("data/sub/nested");
    let deep = fs.path("data/sub/deeper/nested");
    support::bookend(&shallow, FILE_MIB);
    support::bookend(&deep, FILE_MIB);

    let before = support::checksums(fs.root());
    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&fs);
    for file in [&shallow, &deep] {
        let (allocated, used) = support::file_totals(&after, file);
        assert_eq!(allocated, used, "{} was not rewritten", file.display());
    }
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// The copy is an O_TMPFILE, which never makes a directory entry, so nothing
/// can be stranded: not at the top of the mount where copies are made, and
/// not next to any holder, a read-only snapshot's included.
#[test]
fn no_temporary_file_is_left_behind() {
    let fs = Fs::new();
    fs.dir("data");
    support::bookend(&fs.path("data/bookend"), FILE_MIB);
    fs.subvolume("live");
    fs.dir("live/data");
    support::bookend(&fs.path("live/data/bookend"), FILE_MIB);
    fs.snapshot("live", "snapshot");

    let before = support::listing(fs.root());
    let (_, report) = support::apply(&fs);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert!(!report.rewritten.is_empty(), "no rewrite worked");

    assert_eq!(
        before,
        support::listing(fs.root()),
        "the filesystem gained or lost entries"
    );
}

/// A dry run walks the same worklist in the same order and writes nothing,
/// in a read-only snapshot or anywhere else.
#[test]
fn a_dry_run_changes_nothing() {
    let fs = Fs::new();
    fs.subvolume("live");
    fs.dir("live/data");
    let file = fs.path("live/data/bookend");
    support::bookend(&file, FILE_MIB);
    fs.snapshot("live", "snapshot");
    let snapshotted = fs.path("snapshot/data/bookend");

    let before = support::checksums(fs.root());
    let listing = support::listing(fs.root());
    let extents = support::physical_extents(&file);

    let (_, report) = support::dryrun(&fs);
    assert!(!report.rewritten.is_empty(), "it should have found work");
    assert!(report.skipped.is_empty());
    assert!(report.modified.is_empty());

    assert_eq!(before, support::checksums(fs.root()), "contents changed");
    assert_eq!(listing, support::listing(fs.root()), "the listing changed");
    for file in [&file, &snapshotted] {
        assert_eq!(
            extents,
            support::physical_extents(file),
            "{}'s extents moved",
            file.display()
        );
    }
}

/// On a compressed filesystem an extent's on-disk size is not its uncompressed
/// size, and what a reference uses has to be counted in on-disk bytes.
#[test]
fn compressed_extents_are_counted_on_disk() {
    let fs = Fs::with_options(2048, "compress=zstd:3");
    fs.dir("data");
    let file = fs.path("data/compressed");
    support::write_compressible(&file, 4);
    // Inside one compressed extent, which btrfs caps at 128 KiB uncompressed,
    // so this leaves a partly-referenced extent rather than dropping a whole one.
    support::punch_hole(&file, 16 * 1024, 32 * 1024);

    let scan = support::scan(&fs);
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
    fs.dir("data");

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

    let before = support::checksums(fs.root());
    let scan = support::scan(&fs);
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

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    // The point of the test: not that the rewrite reported success, but that
    // every holder actually moved and the extent is gone.
    let after = support::scan(&fs);
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
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
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
    fs.dir("data");

    // 128 MiB is btrfs's cap, and a write that sits on it can come back split,
    // so take whichever extent it gave us the most of and slice that one.
    let big = fs.path("data/big");
    support::write_random_one_extent(&big, 128);
    let biggest = support::file_extents(&big)
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

    let before = support::checksums(fs.root());
    let scan = support::scan(&fs);
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

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&fs);
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
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
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
    fs.dir("data");

    // One large extent per round, each sliced by every holder, so each holder
    // ends up referencing a sector of all of them.
    let mut addresses = Vec::new();
    let holders: Vec<_> = (0..HOLDERS)
        .map(|h| fs.path(&format!("data/photo{h}.jpg")))
        .collect();
    for e in 0..EXTENTS {
        let big = fs.path(&format!("data/big{e}"));
        support::write_random_one_extent(&big, 128);
        let biggest = support::file_extents(&big)
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

    let before = support::checksums(fs.root());
    let scan = support::scan(&fs);
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

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&fs);
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
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// A large file keeping one sector of an extent it used to own outright, plus a
/// small file holding a sector of the same extent. The two disagree about how
/// file offsets map onto extent offsets, which a deleted big file never shows.
#[test]
fn a_holder_deep_inside_a_large_file_moves_too() {
    let fs = Fs::with_options(4096, "");
    let sectorsize = fs.sectorsize();
    fs.dir("data");

    // Big enough that btrfs gives it more than one extent, so the one we work
    // on starts well into the file and file offset and extent offset diverge.
    let movie = fs.path("data/movie.mov");
    support::write_random(&movie, 200);
    // Not the first extent: the point of the shape is a holder whose file
    // offset is nowhere near its offset inside the extent.
    let biggest = support::file_extents(&movie)
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

    let before = support::checksums(fs.root());
    let scan = support::scan(&fs);
    let worklist = support::worklist(&scan);
    let extent = scan
        .extents
        .get(&address)
        .expect("the extent should survive");
    assert_eq!(extent.refs.len(), 2, "the movie and the photo hold it");
    assert!(support::on_worklist(&worklist, address), "almost all waste");

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&fs);
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
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
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
    fs.dir("data");

    let big = fs.path("data/big");
    support::write_random_one_extent(&big, 128);
    let biggest = support::file_extents(&big)
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

    let before = support::checksums(fs.root());
    let scan = support::scan(&fs);
    let worklist = support::worklist(&scan);
    let extent = scan
        .extents
        .get(&address)
        .expect("the extent should survive");
    assert_eq!(extent.refs.len(), OFFSETS.len(), "every sliver found");
    assert!(support::on_worklist(&worklist, address), "almost all waste");

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&fs);
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
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// Two holders whose references into the extent begin together and end apart.
/// A stretch is as long as the first reference that asked for it, so the shorter
/// one is a part of that stretch rather than the whole of it.
#[test]
fn a_holder_whose_reference_is_shorter_than_the_stretch_moves() {
    let fs = Fs::new();
    let sectorsize = fs.sectorsize();
    fs.dir("data");

    let big = fs.path("data/big");
    support::write_random_one_extent(&big, FILE_MIB);
    let address = support::single_extent(&big);

    // "long" holds two sectors of the stretch, "short" only the first of them.
    let long = fs.path("data/long.jpg");
    let short = fs.path("data/short.jpg");
    support::sliver_of(&big, 8 * MIB, &long, MIB, 8192, 2 * sectorsize);
    support::sliver_of(&big, 8 * MIB, &short, MIB, 8192, sectorsize);
    std::fs::remove_file(&big).expect("remove the big file");
    support::sync_fs();

    let before = support::checksums(fs.root());
    let scan = support::scan(&fs);
    let extent = scan
        .extents
        .get(&address)
        .expect("the extent should survive");
    assert_eq!(extent.refs.len(), 2, "both holders should be found");

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&fs);
    assert!(
        !after.extents.contains_key(&address),
        "the extent is still there"
    );
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// A live stretch tens of MiB long, held through a single reference: all of it
/// has to be copied and redirected or the holder keeps the extent alive.
///
/// Nothing else in the suite keeps a stretch this large, so a rewrite that
/// mishandles a long staged copy shows up here.
#[test]
fn a_long_live_stretch_moves_whole() {
    let fs = Fs::with_options(4096, "");
    fs.dir("data");

    let movie = fs.path("data/movie.mov");
    support::write_random_one_extent(&movie, 200);
    let biggest = support::file_extents(&movie)
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

    let before = support::checksums(fs.root());
    let scan = support::scan(&fs);
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

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&fs);
    assert!(
        !support::physical_extents(&movie).contains(&address),
        "the movie still references the extent",
    );
    assert!(
        !after.extents.contains_key(&address),
        "the extent is still there"
    );
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// The same long stretch, with a deduplicator's sliver sitting one sector deep
/// inside it. That sector is held by a file which holds nothing else, so a
/// rewrite that only redirects the holder it started from leaves the sliver's
/// file pointing at the extent.
#[test]
fn a_holder_deep_inside_a_long_stretch_moves() {
    let fs = Fs::with_options(4096, "");
    fs.dir("data");

    let movie = fs.path("data/movie.mov");
    support::write_random_one_extent(&movie, 200);
    let biggest = support::file_extents(&movie)
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

    let before = support::checksums(fs.root());
    let scan = support::scan(&fs);
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

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&fs);
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
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// A file whose size is not a whole number of sectors. Its last reference runs
/// past the end of its data, so the stretch to copy is longer than there is
/// anything to read, and the dedupe which redirects it ends mid-sector.
#[test]
fn a_holder_with_a_short_last_block_moves() {
    let fs = Fs::new();
    fs.dir("data");

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

    let before = support::checksums(fs.root());
    let scan = support::scan(&fs);
    let worklist = support::worklist(&scan);
    assert!(
        support::job_for(&worklist, &file).is_some(),
        "the file should be worth rewriting"
    );

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let after = support::scan(&fs);
    let (allocated, used) = support::file_totals(&after, &file);
    assert_eq!(allocated, used, "nothing should be wasted afterwards");
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// A layout nobody designed: files, holes, reflinks, deduplicator slivers,
/// ragged truncations, read-only snapshots and reflinks into another
/// subvolume, at random offsets in a random order, then the whole run over
/// whatever came out.
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
        support::random_layout(&fs, &mut rng);

        let before = support::checksums(fs.root());
        // apply() re-reads every holder of every extent the report calls freed,
        // and fails if one still points at it.
        let (_, report) = support::apply(&fs);
        assert!(
            report.modified.is_empty(),
            "seed {seed:#x}: {:?}",
            report.modified
        );
        assert!(
            report.skipped.is_empty(),
            "seed {seed:#x}: {:?}",
            report.skipped
        );
        assert_eq!(
            before,
            support::checksums(fs.root()),
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
    fs.dir("data");
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

    let before = support::checksums(fs.root());
    let scan = support::scan(&fs);
    let worklist = support::worklist(&scan);
    assert!(
        support::on_worklist(&worklist, extent),
        "the hole is most of it"
    );

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);

    let after = support::scan(&fs);
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
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// A file ending mid-sector, and a second file whose reference starts at the
/// next sector of the same extent. The stretch the two make together has a gap
/// at the first file's end that neither can be read through, and what lies past
/// it has to be copied all the same.
#[test]
fn a_stretch_with_a_gap_at_a_files_end_moves_whole() {
    let fs = Fs::new();
    let sectorsize = fs.sectorsize();
    fs.dir("data");
    let (short, other) = (fs.path("data/short"), fs.path("data/other"));
    support::write_random_one_extent(&short, FILE_MIB);
    support::reflink(&short, &other);

    // `short` keeps two sectors of the extent, only part of the second readable.
    support::truncate(&short, sectorsize + 1000);
    // `other` takes over from the third sector, up to the first MiB.
    support::punch_hole(&other, 0, 2 * sectorsize);
    support::punch_hole(&other, MIB, (FILE_MIB - 1) * MIB);

    let extent = support::single_extent(&short);
    assert_eq!(
        support::single_extent(&other),
        extent,
        "fixture: the two files should hold one extent between them"
    );

    let before = support::checksums(fs.root());
    let scan = support::scan(&fs);
    assert_eq!(
        scan.extents[&extent].live_ranges,
        vec![0..MIB],
        "fixture: the two references should make one stretch"
    );

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert!(report.rewritten.contains(&extent), "it was not rewritten");
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// Runs over an extent that space reserved past `file`'s end holds, which no
/// dedupe can reach. It has to be left alone before anything is copied.
fn assert_held_past_the_end_is_left_alone(fs: &Fs, file: &Path) {
    let extent = support::single_extent(file);
    let before = support::checksums(fs.root());
    let scan = support::scan(fs);
    assert!(
        support::on_worklist(&support::worklist(&scan), extent),
        "fixture: the extent should look worth rewriting"
    );

    let options = support::options(fs.root(), true, false);
    let (stats, report) = btrealloc::run(&options).expect("run over the fixture");
    assert!(report.rewritten.is_empty(), "{:x?}", report.rewritten);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert_eq!(stats.left_alone.get(LeftAlone::PastEnd).count, 1);
    assert_eq!(
        support::physical_extents(file),
        vec![extent],
        "nothing should have been copied"
    );
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}

/// Space reserved wholly past a file's end, left of a reservation the file
/// filled the start of.
#[test]
fn an_extent_held_past_a_files_end_is_left_alone() {
    let fs = Fs::new();
    fs.dir("data");
    let file = fs.path("data/reserved");
    support::preallocate_past_end(&file, FILE_MIB * MIB);
    support::write_random_into(&file, MIB);
    // Most of the reservation goes, the last MiB of it stays.
    support::punch_hole(&file, MIB, (FILE_MIB - 2) * MIB);
    let offsets: Vec<u64> = support::file_extents(&file)
        .iter()
        .map(|r| r.file_offset)
        .collect();
    assert_eq!(
        offsets,
        vec![0, (FILE_MIB - 1) * MIB],
        "fixture: the file should hold its first MiB and the last of the reservation"
    );

    assert_held_past_the_end_is_left_alone(&fs, &file);
}

/// A reservation the file has grown into only part of, so one reference runs
/// from inside the file to past its end.
#[test]
fn an_extent_held_across_a_files_end_is_left_alone() {
    let fs = Fs::new();
    fs.dir("data");
    let file = fs.path("data/reserved");
    support::preallocate_past_end(&file, FILE_MIB * MIB);
    // Two MiB of the reservation stays, and the file grows over the first.
    support::punch_hole(&file, 2 * MIB, (FILE_MIB - 2) * MIB);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&file)
        .and_then(|f| f.set_len(MIB))
        .expect("grow the file");
    support::sync_fs();
    let refs = support::file_extents(&file);
    assert!(
        refs.len() == 1 && refs[0].num_bytes == 2 * MIB,
        "fixture: one reference should run across the file's end"
    );

    assert_held_past_the_end_is_left_alone(&fs, &file);
}

/// A single compressible sector left of a large extent, on a compressing mount.
/// Its copy compresses small enough to be stored inline, and an inline extent
/// is not something a holder can be pointed at.
#[test]
fn a_compressible_sector_alone_in_its_extent_moves() {
    let fs = Fs::with_options(2048, "compress=zstd:3");
    let sectorsize = fs.sectorsize();
    fs.dir("data");
    let file = fs.path("data/sector");

    let mut data = vec![0u8; (FILE_MIB * MIB) as usize];
    support::Rng::new(1).fill(&mut data);
    let kept = 8 * MIB;
    data[kept as usize..(kept + sectorsize) as usize].fill(b'a');
    // Written into a reservation, which is never compressed: one extent.
    support::write_one_extent(&file, &data);
    support::punch_hole(&file, 0, kept);
    support::punch_hole(&file, kept + sectorsize, FILE_MIB * MIB - kept - sectorsize);
    let extent = support::single_extent(&file);

    let before = support::checksums(fs.root());
    let scan = support::scan(&fs);
    assert!(
        support::on_worklist(&support::worklist(&scan), extent),
        "fixture: the extent should be worth rewriting"
    );

    let (_, report) = support::apply(&fs);
    assert!(report.modified.is_empty(), "{:?}", report.modified);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert!(report.rewritten.contains(&extent), "it was not rewritten");
    assert!(
        !support::physical_extents(&file).contains(&extent),
        "the file still references the extent"
    );
    assert_eq!(before, support::checksums(fs.root()), "contents changed");
}
