# Working on btrealloc

This repo uses a Nix flake. The Rust toolchain (cargo, rustc, clippy, rustfmt) lives in the dev shell. The flake like `nix develop` should already have been activated with direnv.

Commands:

```sh
cargo build
cargo test
cargo fmt
cargo clippy
```

## The VM test suite

`tests/vm.rs` is the end-to-end suite. It needs root (btrfs ioctls, loop mounts) so `cargo test` skips it. Run it in its own NixOS VM:

```sh
nix run .#checks.x86_64-linux.vm.driver   # also: ./runvm.sh
```

`nix flake check` runs the same thing.
