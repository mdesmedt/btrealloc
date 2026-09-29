# The btrealloc package, for `callPackage`. The flake builds it this way, and so
# can a NixOS config that doesn't use flakes, against its own nixpkgs.
{ lib, rustPlatform }:

let
  cargoToml = lib.importTOML ./Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  inherit (cargoToml.package) version;

  # Only what cargo needs, so a path checkout doesn't copy target/ into the
  # store. tests/ is here because Cargo.toml names the VM suite's source.
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
      ./tests
    ];
  };
  cargoLock.lockFile = ./Cargo.lock;

  meta = {
    description = "Reclaim unreachable space on btrfs by rewriting live data into compact extents";
    homepage = "https://github.com/mdesmedt/btrealloc";
    license = with lib.licenses; [
      mit
      asl20
    ];
    mainProgram = "btrealloc";
    platforms = lib.platforms.linux;
  };
}
