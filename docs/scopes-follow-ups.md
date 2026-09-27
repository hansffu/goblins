# Open scope issues

Recorded 2026-09-27 after trying scopes in a real project. The approaches below
are questions, not decisions.

## Agents do not discover on-demand Docker

Codex, running in a goblin whose scope enables Docker, ran a Gradle build with
Testcontainers integration tests. Testcontainers failed with "Could not find a
valid Docker environment". Codex reported that `DOCKER_HOST` points to
`/run/goblins/docker-socket/docker.sock` but the socket does not exist, and
concluded the environment had no Docker. It never ran `goblins enable-docker`,
so the tests were skipped even though Docker was one approval away.

Contributing causes:

- No agent skill mentions Docker. The installed skills (`goblins`,
  `goblins-packages`, `goblins-spawn`, `goblins-messaging`) cover packages,
  children and messaging only.
- `DOCKER_HOST` is set from startup, before the engine is attached, so the
  environment looks like a broken Docker setup rather than an available one.
- Nothing at the failure point says how to get Docker: the socket path is
  simply missing.

Possible approaches:

- Describe `goblins enable-docker` in an agent skill, or a scope-provided skill
  installed only when the goblin's scope enables Docker.
- Put a placeholder at the socket path that answers with an explanatory error
  (for example "run `goblins enable-docker` to attach this sandbox to the
  scope's Docker engine") until the engine is attached.
- Report scope and Docker availability in `goblins status` in a way agents
  already check, for example "Docker: available (run goblins enable-docker)".
- Leave `DOCKER_HOST` unset until Docker is enabled. This changes the existing
  Ryuk/Testcontainers startup behavior and needs checking.
- Configure affected goblins with `docker.enable = true`, which attaches at
  startup and avoids discovery entirely.

Validate with Codex and Claude Code running a Testcontainers build in a scope
with Docker enabled but not yet attached.
