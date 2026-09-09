# The end to end suite, run in a throwaway VM because everything it does needs
# root: the search ioctl btrealloc reads extents through, and the mkfs and loop
# mount behind every test's own filesystem.
#
# `src` is the flake source, which is both what the tests are built from and
# what they test.
{ pkgs, src }:

let
  # The suite built as a binary, so the VM can run it without a toolchain or a
  # source tree. `cargo test` leaves this target alone, so it has to be asked
  # for by name.
  vmtest = pkgs.rustPlatform.buildRustPackage {
    pname = "btrealloc-vmtest";
    version = "0.1.0";
    inherit src;
    cargoLock.lockFile = ../Cargo.lock;
    nativeBuildInputs = [ pkgs.jq ];
    buildPhase = ''
      runHook preBuild
      cargo test --release --test vm --no-run --message-format=json \
        > cargo-test.json
      runHook postBuild
    '';
    # Nothing to check: running the suite is what the VM is for.
    doCheck = false;
    installPhase = ''
      runHook preInstall
      install -Dm755 \
        "$(jq -r 'select(.profile.test == true) | .executable' cargo-test.json | head -1)" \
        $out/bin/btrealloc-vmtest
      runHook postInstall
    '';
  };
in
pkgs.testers.runNixOSTest {
  name = "btrealloc";

  nodes.machine = {
    environment.systemPackages = [
      vmtest
      pkgs.btrfs-progs
    ];
    boot.supportedFilesystems = [ "btrfs" ];
    # Each test's filesystem lives in a tmpfs, so it is spent from this.
    virtualisation.memorySize = 6144;
  };

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    # One at a time: each test holds a filesystem's worth of tmpfs while it
    # runs, and they would otherwise be held all at once.
    print(machine.succeed("btrealloc-vmtest --test-threads=1 --nocapture 2>&1"))
  '';
}
