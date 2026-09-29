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
        btrealloc = pkgs.callPackage ./package.nix { };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.btrealloc;
      });

      # Adds btrealloc to a NixOS config's own nixpkgs.
      overlays.default = final: prev: {
        btrealloc = final.callPackage ./package.nix { };
      };

      devShells = forAll (
        pkgs: with pkgs; {
          default = mkShell {
            packages = [
              cargo
              rustc
              rust-analyzer
              clippy
              rustfmt
            ];
          };
        }
      );

      checks = forAll (pkgs: {
        # The end to end suite, in a VM of its own. `nix flake check` runs it.
        vm = import ./nix/vmtest.nix {
          inherit pkgs;
          src = self;
        };
      });
    };
}
