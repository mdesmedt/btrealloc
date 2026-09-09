{
  description = "btrealloc - reclaim unreachable space on btrfs by rewriting live data into compact extents";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: {
        btrealloc = pkgs.rustPlatform.buildRustPackage {
          pname = "btrealloc";
          version = "0.1.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
        };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.btrealloc;
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.rust-analyzer
            pkgs.clippy
            pkgs.rustfmt
          ];
        };
      });

      checks = forAll (pkgs: {
        # The end to end suite, in a VM of its own. `nix flake check` runs it.
        vm = import ./nix/vmtest.nix {
          inherit pkgs;
          src = self;
        };
      });
    };
}
