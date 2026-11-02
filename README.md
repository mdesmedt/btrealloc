# Btrealloc - Btrfs extent reallocator to reclaim disk space

Btrfs is a Copy-on-Write filesystem which stores files on-disk using extents. For example, a 100MiB file could precisely be stored as 100x 1MiB extents. However, Copy-on-Write means that if a write takes place inside the file it does not modify these bytes in-place. A small 1kB write inside the file will likely trigger the allocation of a new extent to store these bytes on disk. The file's metadata would now reflect it pointing to the 100 original extents for the majority of its data, and one new extent holding 1kB. However, this means that 1kB of the previously full 1MiB extent has now become "unreachable". This wastes a small amount of space.

With repeated writes, worst-case situations can occur where a file holds on to only a sliver of a large extent, wasting the rest of its space. This can grow to gigabytes with enough churn.

[btdu](https://github.com/CyberShadow/btdu) is a powerful sampling profiler for Btrfs filesystems which can quickly visualize this amount of "unreachable" space.

There are some ways to attempt to reclaim unreachable space:

1. `btrfs filesystem defragment`: The defragmentation operation does not specifically reallocate extents with "unreachable" space but it might do some of this as a side-effect. A problem is that it's more likely to do this when specifying large extent sizes, e.g. `-t 128M` but reallocating with large extent sizes as a target also carries a higher likelihood of forming more unreachable allocations down the line.
2. `cp --reflink=never`: By copying files with `--reflink=never` new, most likely better fitting, extents are allocated for the file. Removing the old file then releases its ownership of possibly large extents with unreachable space, which the filesystem could garbage collect once they are no longer referenced. This method can work but it is not an in-place operation and requires possibly a large amount of free space to temporarily copy the files to.

Both of the above operations also unshare any deduplication which was established with tools like `bees` or `duperemove`.

This tool is specifically designed to give the filesystem an opportunity to reallocate extents with unreachable data, in-place, without modifying the original file's contents or metadata, and preserving existing deduplication.

Btrealloc works on a whole filesystem at once, every subvolume and snapshot included, read-only snapshots too.

# Usage

Btrealloc needs the filesystem's top-level subvolume (`subvolid=5`) mounted, and is given that mount point. Only from there can it reach every subvolume and snapshot. It works through a private, detached copy of that mount (`open_tree`), so anything mounted over parts of it does not get in the way, and nothing is left mounted when it exits, however it exits. If you normally only mount subvolumes, mount the root subvolume first:

```
sudo mkdir -p /mnt/rootsubvolume
sudo mount -o subvolid=5,noatime /dev/disk/by-uuid/<UUID> /mnt/rootsubvolume
```

By default the tool generates a report only and does not make any changes:

```
Usage: btrealloc [OPTIONS] <MOUNT>

Arguments:
  <MOUNT>  Where the filesystem's top-level subvolume is mounted (`mount -o subvolid=5`). Every subvolume and snapshot on it is scanned from there

Options:
      --apply     Actually perform the extent reallocation operation, writing changes to the filesystem
  -v, --verbose   Also say why each extent was left alone, and list each extent rewritten, or that would be without --apply, with every file holding it
  -j, --jobs <N>  Threads to resolve extents on. Defaults to one per CPU
  -h, --help      Print help
```

A run that finishes exits 0 even when some extents were left alone, including ones whose rewrite failed: on a filesystem in use files change or get deleted, and each one is reported as it happens and in the table at the end. It exits 1 when it cannot finish, including when it stops because the filesystem is out of space, over a quota, read-only or failing.

# Example

Generate a report:

```
sudo btrealloc /mnt/filesystem
Scanning: /mnt/filesystem
extents:            2723877
allocated:          1010.22 GiB
used:               957.03 GiB
unreachable:        46.50 GiB
  reclaimable:      44.07 GiB, copying 18.85 GiB from 6058 extents
  left alone:       2.43 GiB
...

Nothing written. Run with --apply to reallocate extents.
```

Reallocating the extents with `btrealloc --apply`:

```
sudo btrealloc /mnt/filesystem --apply
Scanning: /mnt/filesystem
rewriting extents as they are found
...
copied 18.85 GiB to free 44.07 GiB from 6058 extents
```

Generating the report again:

```
sudo btrealloc /mnt/filesystem
Scanning: /mnt/filesystem
extents:            2756285
allocated:          964.73 GiB
used:               955.57 GiB
unreachable:        2.45 GiB
```

# How it works

## Scanning

- The tool walks the filesystem's extent tree in address order. Every extent is iterated over exactly once.
- For each extent, it finds every file extent item referencing it, in any subvolume. The extent tree names most of them directly. Where it cannot, because the references go through tree blocks shared with a snapshot, the kernel's backreference walk (`LOGICAL_INO`) finds them.
- For each extent, it calculates how much of the extent is being actively used. The extent's data is split into:
  - Allocated: The total size of the extent
  - Used: The amount of bytes which are being used by any file
  - Unreachable: How many bytes in the extent are no longer being referenced by any file
- Extents allocated after the run started, including new ones created for the reallocation process, are passed over.

## Selection

- Each extent found is checked against a set of criteria, and skipped:
  - With small amounts of reclaimable space (currently <64kb)
  - Which require a large copy to free up a relatively small amount of space (currently >4:1 ratio)
  - Held by a nodatacow file, which cannot be deduped

## Applying

- Extents which are selected to be applied are now processed to attempt to reclaim unreachable space. The process:
  - Look up every file holding the extent again, with the kernel's backreference walk, since earlier rewrites can have changed it
  - Find each file by its subvolume and inode number, and check the file opened is still that inode
  - Iterate over all referenced ranges in the extent, finding "live ranges" of referenced bytes
  - For each live range an anonymous temp file is created with `O_TMPFILE` at the top of the mount and it is copied there
  - For each file which holds the original extent, call `FIDEDUPERANGE` to point its chunks from the old extent into the new extents. This operation is what actually modifies the file. The kernel guarantees that:
    - The operation is atomic
    - The bytes in the source range and the destination range are identical
  - Release the anonymous temp file
  - If all goes well, the original extents (with unreachable bytes) are now no longer referenced and can be garbage-collected
- A failure that concerns one extent leaves it alone and moves on. One that means no rewrite after it can succeed ends the run with an error: the filesystem is out of space, over a quota, has gone read-only, or reports I/O errors or corruption

## Effects on the filesystem

- As this process completes, the previously underutilized extents can now be garbage collected by the kernel.
- Disk utilization might temporarily be higher while the above work happens but it should go down when enough underutilized extents are freed.
- The files touched should only have had their extents reallocated. Their contents and metadata should remain untouched.
- Files in read-only snapshots are rewritten too. The snapshot stays read-only and its contents and times do not change, but its extents do. If a `btrfs send` of a snapshot is running the kernel refuses to dedupe into it and those extents are skipped for that run.
- Rewriting an extent that many snapshots share writes to the metadata of every one of them, and unshares metadata the snapshots shared until then. With many snapshots, the metadata this costs can outweigh the data freed.
- Compressed extents are read, decompressed and written back, where btrfs may compress the data again. Existing deduplication between files is preserved. However wshich on-disk bytes of a compressed extent belong to which part of its data cannot be known, so for compressed extents "used" and "unreachable" are estimated in proportion to the data still referenced. A live range compressed on its own may also compress worse than it did as part of the whole extent so the space actually freed can be less than reported.
- Currently each live range gets allocated a new extent. This can create more extents and more metadata. Coalescing live ranges into a single extent could be a future improvement.

# Test infrastructure

Basic unit tests with Rust cargo:

```
cargo test
```

Because actually scanning a filesystem with this tool or doing actual work with `--apply` requires root access, a more complete test suite runs sandboxed in a NixOS VM. With `nix` available, run `nix flake check` or:

```
./runvm.sh
```

# License

[MIT License](LICENSE)
