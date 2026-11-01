{ pkgs }:
let
  # Moby needs mkfs.xfs, not xfsprogs' Python/DBus maintenance scripts. Copy
  # the native executable so on-demand availability preserves a Python-free
  # minimal Goblins runtime (a symlink would retain the entire bin output).
  xfsTools = pkgs.runCommand "goblins-docker-xfs-tools" { } ''
    mkdir -p $out/bin
    cp ${pkgs.lib.getBin pkgs.xfsprogs}/bin/mkfs.xfs $out/bin/
  '';
in
# Moby supports --rootless independently of RootlessKit for OCI/cgroups, but
# its network controller unconditionally constructs a RootlessKit API client.
# Our launcher supplies the isolated network, so no RootlessKit port mapping
# service exists. Skip that API and let the native launcher keep BuildKit in
# the inherited outer cgroup namespace (Docker's daemon flag covers run, but
# not BuildKit). The inherited seccomp filter enforces the cgroup restriction.
(pkgs.docker.override { xfsprogs = xfsTools; }).moby.overrideAttrs (old: {
  patches = (old.patches or [ ]) ++ [ ./docker-native-rootless.patch ];
})
