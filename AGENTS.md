# Working on btrealloc

This repo uses a Nix flake. The Rust toolchain (cargo, rustc, clippy, rustfmt)
lives in the dev shell, not on `PATH`. Prefix commands with `nix develop`:

```sh
nix develop -c cargo build
nix develop -c cargo test        # unit + integration tests (no root needed)
nix develop -c cargo fmt
nix develop -c cargo clippy
```

## The VM test suite

`tests/vm.rs` is the end-to-end suite. It needs root (btrfs ioctls, loop mounts)
so `cargo test` skips it. Run it in its own VM:

```sh
nix run .#checks.x86_64-linux.vm.driver   # also: ./runvm.sh
```

`nix flake check` runs the same thing.
