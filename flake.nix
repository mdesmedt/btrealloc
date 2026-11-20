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
      packages = forAll (pkgs: rec {
        btrealloc = pkgs.rustPlatform.buildRustPackage {
          pname = "btrealloc";
          version = (pkgs.lib.importTOML ./Cargo.toml).package.version;

          # Only what cargo needs, so edits to docs or Nix files don't rebuild
          # the package or rerun the VM suite.
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
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
            license = pkgs.lib.licenses.mit;
            mainProgram = "btrealloc";
            platforms = pkgs.lib.platforms.linux;
          };
        };
        default = btrealloc;
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rust-analyzer
            clippy
            rustfmt
            cargo-deny
          ];
        };
      });

      checks = forAll (pkgs: {
        # The end to end suite, in a VM of its own. `nix flake check` runs it.
        vm = import ./nix/vmtest.nix {
          inherit pkgs;
          inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) btrealloc;
        };
      });
    };
}
