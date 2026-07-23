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
short: the role's own worktree and the base repo's `.git` (read-write, so
git operations work), plus the host's `~/.claude` (read-only, the only
credential surface) -- nothing else of the host is ever reachable from
inside the container (no `~/.ssh`, `~/.aws`, `~/.config/gh`, `.env`, or the
rest of the host's real `$HOME`).

## Accepted v1 limits

- **No egress filtering.** The container runs on Docker's default bridge
  network, so it can reach the Anthropic API. `git push origin` still fails
  by construction (no credentials are ever mounted/configured), but nothing
  prevents outbound connections to other hosts. Deferred to a follow-up --
  see ADR-0019.
- **Crash recovery is still pid-based.** Recovering a run after `warden`
  itself crashes mid-run kills a host pid (the `docker run` client's own
  pid); it does not yet reach into the daemon to reclaim an orphaned
  container by name. `Sandbox::destroy` reliably removes a container on
  every teardown path this crate controls (normal exit, cancellation,
  explicit destroy) -- only the crash-recovery path itself does not use it
  yet. See ADR-0015/ADR-0019.
