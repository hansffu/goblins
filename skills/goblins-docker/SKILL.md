---
name: goblins-docker
description: Attach a Goblins sandbox to its scope's Docker engine with goblins enable-docker. Use before concluding Docker is unavailable, when work needs Docker, containers, Docker Compose or Testcontainers, or on "Could not find a valid Docker environment", a missing /run/goblins/docker-socket/docker.sock or "docker: command not found".
---

Docker in a Goblins sandbox is on demand. When `DOCKER_HOST` is
`unix:///run/goblins/docker-socket/docker.sock`, this goblin's scope provides a
Docker engine, but it is not attached until approved. Until then the socket does
not exist and the `docker` command may be missing. This is expected. It does not
mean the environment has no Docker or is misconfigured.

To attach, run
`goblins enable-docker --reason "REASON"`, stating briefly which user task needs
containers. The command waits for the human decision and engine readiness. Exit
0 means the socket is live and the Docker CLI is installed. Already-running
shells and tools need no restart, because `DOCKER_HOST` does not change.
Exit 1 means denial or a known failure, such as a goblin without a scope or a
scope that does not enable Docker. Report the result and do not retry.
Exit 2 means the outcome is unknown; do not automatically submit another
request, because the original may still complete. `goblins status` shows the
goblin's scope and whether Docker is enabled.

If `DOCKER_HOST` is unset, this goblin has no Docker engine. Say so instead of
requesting approval. Never substitute `goblins request-package docker` or start
your own daemon: a Docker client package provides no engine. Do not change
`DOCKER_HOST` or the Testcontainers settings Goblins provides.

After a successful attachment, rerun the command that needed Docker. Force a
rerun of tests the build skipped or cached while Docker was missing, for example
with Gradle `--rerun-tasks`. Published container ports are reachable on
`localhost`.
