# `--isolation docker` reference image

`warden run --isolation docker` runs every agent invocation inside a
container instead of directly on the host (`warden_sandbox::DockerSandbox`,
issue #49). See `crates/warden-sandbox/src/docker.rs`'s own module docs for
the exact mount/network/auth guarantees this backend provides -- this file
is only about the image itself.

## Building the image

```sh
docker build -t warden-agent:latest crates/warden-sandbox/docker
```

`warden-agent:latest` is `--isolation docker`'s own default image
(`DEFAULT_DOCKER_IMAGE` in `crates/warden/src/main.rs`) -- build it under
that exact tag and no further flag is needed. `--isolation-image` overrides
it, for a locally customized image or a different tag.

## What the image needs to contain

Nothing `DockerSandbox` itself depends on beyond two things being on `PATH`
inside the container:

- **`git`** -- every role's own worktree is a real `git worktree`; the
  coder/reviewer/tester all shell out to `git` themselves from inside the
  container.
- **Whatever CLI the run's `--tool` adapter execs** -- `claude`
  (`@anthropic-ai/claude-code`, a Node CLI) for `--tool claude`, the only
  adapter today.

The provided `Dockerfile` builds exactly that, on top of `node:20-slim`.

## How `--isolation docker` finds the image

`DockerSandbox::execute` passes `DockerConfig::image` straight through to
`docker run <image> ...` -- normal Docker image resolution applies (a local
tag first, a registry pull if that tag isn't present locally and looks
pull-able). There is no separate "image exists" pre-check: a missing image
surfaces as `docker run`'s own non-zero exit / stderr, the same way any
other `docker run` failure would.

## What is (and is not) mounted into the container

See `crates/warden-sandbox/src/docker.rs`'s own module docs for the
authoritative, up-to-date list and the reasoning behind each mount. In
short: the role's own worktree and the base repo's `.git` (read-write), plus
the host's `~/.claude` (read-only). No other host path is bind-mounted.

## Security model

- `~/.claude` is read-only, not secret from the agent. Default bridge
  networking permits exfiltration of Claude credentials and repository data.
- `.git` is writable because linked worktrees require it. Agents can mutate
  repository metadata; host-side git commands disable repository hooks.
- Containers drop all Linux capabilities except `CAP_DAC_OVERRIDE`, required
  to write host-owned worktree and `.git` bind mounts on Linux. They set
  `no-new-privileges` and limit processes to 256. CPU and memory remain unbounded.
- Docker daemon and host kernel remain trusted. This backend is not a boundary
  against daemon compromise or container-runtime/kernel vulnerabilities.
- No egress allowlist exists. Agent APIs require network access; deploy an
  outbound proxy or Docker network policy when domain restriction is needed.

## Crash recovery

Every agent container carries managed/run labels. Startup recovery queries
Docker by run label and force-removes matching containers before deleting
orphan worktrees. Cleanup errors are explicit; a daemon unavailable during
that pass requires manual cleanup after Docker returns.
