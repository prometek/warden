# Reference image for `warden run --isolation docker` (issue #49).
#
# `claude` (`@anthropic-ai/claude-code`) is a Node CLI, so this is based on
# the official `node` image rather than a bare distro. `git` is required --
# every role's own worktree is a real `git worktree`, and the coder/
# reviewer/tester all shell out to `git` themselves (diff, commit, status)
# from inside the container `warden_sandbox::DockerSandbox` runs them in.
#
# This image is deliberately *not* the security boundary -- `DockerSandbox`
# itself is (the bind mounts it chooses, documented in
# `crates/warden-sandbox/src/docker.rs`). This Dockerfile only needs to make
# sure the tools every role invokes are actually present and on `PATH`.
FROM node:20-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && npm install -g @anthropic-ai/claude-code

# `warden_sandbox::docker`'s own docs: the container's `HOME` is always
# `/root` (this image runs as its default `root` user, unmodified) -- kept
# consistent with `DockerSandbox`'s own `CONTAINER_HOME` constant. Do not
# add a `USER` directive here without updating that constant to match.
