# Warning

- **This code is a prototype and not extensively tested or validated!**
- **Running this tool modifies your filesystem. It might cause data loss or corruption.**
- **Do not use it unless you are sure you know what you're doing.**
- **Do not use it on data you care about unless you have backups.**
- **This code comes with no warranty or support.**
- **Use at your own risk.**

# Btrealloc - Btrfs extent reallocator to reclaim disk space

Btrfs is a Copy-on-Write filesystem which, in short, stores files on-disk using *extents*. For example, a 100MiB file could precisely be stored as 100x 1MiB extents without wasting bytes. The COW part of Btrfs means that if a write takes place inside the file, it does not modify these bytes in-place. A 1kB write likely triggers the allocation of a new extent to store it on disk. The file's metadata would now reflect it pointing to the 100 original extents for the majority of its data, and one new extent holding this 1kB write. However, this means that 1kB of the previously full 1MiB extent has now become "unreachable". This wastes a small amount of space.

However with repeated writes, worst-case situations can occur where files hold on to a sliver of a large extent, wasting the rest of its space. This can grow to gigabytes with enough churn. [btdu](https://github.com/CyberShadow/btdu) is a powerful sampling profiler for Btrfs filesystems which can quickly visualize this amount of "unreachable" space.

There are some ways to attempt to reclaim unreachable space:

1. `btrfs filesystem defragment`: The defragmentation operation does not specifically reallocate extents with "unreachable" space but it might do some of this as a side-effect. A problem is that it's more likely to do this when specifying large extent sizes, e.g. `-t 128M` but reallocating with large extent sizes as a target also carries a higher likelihood of forming more unreachable allocations down the line.
2. `cp --reflink=never`: By copying files with `--reflink=never` new, most likely better fitting, extents are allocated for the file. Removing the old file then releases its ownership of possibly large extents with unreachable space, which the filesystem could garbage collect once they are no longer referenced. This method can work but it is not an in-place operation and requires possibly a large amount of free space to temporarily copy the files to.

Both of the above operations also unshare any deduplication which was established with tools like `bees` or `duperemove`.

This tool is specifically designed to give the filesystem an opportunity to reallocate extents with unreachable data, in-place, without modifying the original file's contents or metadata, and preserving any existing deduplication.

Btrealloc works on a whole filesystem at once, every subvolume and snapshot included, read-only snapshots too. An extent is only freed once every file holding it lets go, so an extent shared with a snapshot is rewritten in the snapshot as well.

# Usage

Btrealloc needs the filesystem's top-level subvolume (`subvolid=5`) mounted, and is given that mount point. Only from there can it reach every subvolume and snapshot. It does not mount anything itself. If you normally only mount subvolumes, mount the root subvolume first:

```
sudo mkdir -p /mnt/rootsubvolume
sudo mount -o subvolid=5,noatime /dev/disk/by-uuid/<UUID> /mnt/rootsubvolume
```

By default the tool generates a report only and does not make any changes:

```
Usage: btrealloc [OPTIONS] <MOUNT>

Arguments:
  <MOUNT>  Where the filesystem's top-level subvolume is mounted

Options:
      --apply    Actually perform the extent reallocation operation, writing changes to the filesystem
  -v, --verbose  Increase logging verbosity
      --dryrun   Perform a dry run without writing any data
  -h, --help     Print help
```

# Example

Generate a report:

```
sudo btrealloc /mnt/filesystem
Scanning: /mnt/filesystem
extents:            2723877
allocated:          1010.22 GiB
used:               957.03 GiB
unreachable:        46.50 GiB
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

## Effects on the filesystem

- As this process completes, the previously underutilized extents can now be garbage collected by the kernel.
- Disk utilization might temporarily be higher while the above work happens but it should go down when enough underutilized extents are freed.
- The files touched should only have had their extents reallocated. Their contents and metadata should remain untouched.
- Files in read-only snapshots are rewritten too. The snapshot stays read-only and its contents and times do not change, but its extents do. If a `btrfs send` of a snapshot is running the kernel refuses to dedupe into it and those extents are skipped for that run.
- Rewriting an extent that many snapshots share writes to the metadata of every one of them, and unshares metadata the snapshots shared until then. With many snapshots, the metadata this costs can outweigh the data freed.
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
