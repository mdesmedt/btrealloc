# The end to end suite, run in a throwaway VM because everything it does needs
# root: the search ioctl btrealloc reads extents through, and the mkfs and loop
# mount behind every test's own filesystem.
#
# `btrealloc` is the flake's package; the suite is built from its source and
# tests that same code.
{ pkgs, btrealloc }:

let
  # The suite built as a binary, so the VM can run it without a toolchain or a
  # source tree. `cargo test` leaves this target alone, so it has to be asked
  # for by name.
  vmtest = btrealloc.overrideAttrs (old: {
    pname = "btrealloc-vmtest";
    nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.jq ];
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
  });
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

    # No need for networking
    virtualisation.vlans = [ ];
  };

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    # One at a time: each test holds a filesystem's worth of tmpfs while it
    # runs, and they would otherwise be held all at once.
    print(machine.succeed("btrealloc-vmtest --test-threads=1 --nocapture 2>&1"))
  '';
}
