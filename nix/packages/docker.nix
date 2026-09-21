{ pkgs }:
# Moby supports --rootless independently of RootlessKit for OCI/cgroups, but
# its network controller unconditionally constructs a RootlessKit API client.
# Our launcher supplies the isolated network, so no RootlessKit port mapping
# service exists. Skip that API and let the native launcher keep BuildKit in
# the inherited outer cgroup namespace (Docker's daemon flag covers run, but
# not BuildKit). The inherited seccomp filter enforces the cgroup restriction.
pkgs.docker.moby.overrideAttrs (old: {
  patches = (old.patches or [ ]) ++ [ ./docker-native-rootless.patch ];
})
