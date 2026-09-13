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

There are some ways to attempt to attempt to reclaim unreachable space:

1. `btrfs filesystem defragment`: The defragmentation operation does not specifically reallocate extents with "unreachable" space but it might do some of this as a side-effect. A problem is that it's more likely to do this when specifying large extent sizes, e.g. `-t 128M` but reallocating with large extent sizes as a target also carries a higher likelihood of forming more unreachable allocations down the line.
2. `cp --reflink=never`: By copying files with `--reflink=never` new, most likely better fitting, extents are allocated for the file. Removing the old file then releases its ownership of possibly large extents with unreachable space, which the filesystem could garbage collect once they are no longer referenced. This method can work but it is not an in-place operation and requires possibly a large amount of free space to temporarily copy the files to.

Both of the above operations also unshare any deduplication which was established with tools like `bees` or `duperemove`.

This tool is specifically designed to give the filesystem an opportunity to reallocate extents with unreachable data, in-place, without modifying the original file's contents or metadata, and preserving any existing deduplication.

**Note:** None of the above mitigations, nor this tool, addresses snapshots. Any extent which is reallocated by defrag/cp/btrealloc will still remain on disk as long as the snapshot exists. Reallocating while extents are held by a snapshot can increase disk usage.

# Usage

By default the tool generates a report only.

```
Usage: btrealloc [OPTIONS] <PATH>

Arguments:
  <PATH>  The file or directory to scan

Options:
      --apply    Actually perform the extent reallocation operation, writing changes to the filesystem
      --verify   Hash every file before and after it is rewritten. This is an optional validation as the kernel already checks chunk equality
  -v, --verbose  Increase logging verbosity
      --dryrun   Perform a dry run without writing any data
  -h, --help     Print help
```

# Example

Generate a report:

```
sudo btrealloc /mnt/drive/stuff/
Scanning: /mnt/drive/stuff/
files:              242138
file size:          1.04 TiB
extents:            2723877
allocated:          1010.22 GiB
used:               957.03 GiB
unreachable:        46.50 GiB
unreachable shared: 6.68 GiB
```

Reallocating the extents with `btrealloc --apply`:

```
sudo btrealloc /mnt/drive/stuff/ --apply
...
rewriting 6058 extents: copying 18.85 GiB to free 44.07 GiB
...
copied 18.85 GiB to free 44.07 GiB from 6058 extents
```

Generating the report again:

```
sudo btrealloc /mnt/drive/stuff/
Scanning: /mnt/drive/stuff/
files:              242138
file size:          1.04 TiB
extents:            2756285
allocated:          964.73 GiB
used:               955.57 GiB
unreachable:        2.45 GiB
unreachable shared: 6.70 GiB
```

# How it works

## Scanning phase

- The tool recursively scans a given path. Either an entire Btrfs volume or a subdirectory.
- For each file found, it retrieves the extents it references to hold the file's data
- For each extent, it calculates how much of the extent is being actively used. The extent's data is split into:
  - Allocated: The total size of the extent
  - Used: The amount of bytes which are being used by the files under the path
  - Reclaimable: How many bytes in the extent are no longer being referenced by any files under the path
  - Reclaimable "shared": The same measure as above, but the extent is also referenced by files outside of our path so we cannot conclusively prove this space is unreachable.

## Selection phase

- A selection pass is done, it skips extents:
  - With small amounts of reclaimable space (currently <64kb)
  - Which require a large copy to free up a relatively small amount of space
  - Shared extents, which are referenced outside of the scanned path

## Apply phase

- All remaining extents are now processed one at a time. For every extent:
  - Create an anonymous temp file with `O_TMPFILE`
  - Iterate over all referenced ranges in the extent, and write them to the temp file. This gives the filesystem a chance to reallocate extents more economically.
  - For each file which holds the original extent, call `FIDEDUPERANGE` to point its chunks from the old extent into the new extents. This operation is what actually modifies the file. The kernel guarantees that:
    - The operation is atomic
    - The bytes in the source range and the destination range are identical
  - Release the anonymous temp file

## Effects on the filesystem

- As this process completes, the previously underutilized extents can now be garbage collected by the kernel.
- Disk utilization might temporarily be higher while the above work happens but it should go down when enough underutilized extents are freed.
- The files touched should only have had their extents reallocated. Their contents and metadata should remain untouched.

# Test infrastructure

Basic unit tests with Rust cargo:

```
cargo test
```

Because doing any actual work with `--apply` in this tool requires root access, a more complete test suite runs sandboxed in a NixOS VM. Run `nix flake check` or `./runvm.sh` to run the tests. Currently only works under NixOS.

# License

[MIT License](LICENSE)
