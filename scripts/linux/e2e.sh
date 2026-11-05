#!/bin/bash
# The Linux CI's end-to-end run, here: what .github/workflows/linux.yml does under Xvfb, in Docker
# on this machine's architecture (arm64 on Apple silicon), against the
# e2e databases already up on the host (`--network host`; the seeds through the host's docker).
#
#   scripts/linux/e2e.sh                         # every stage
#   scripts/linux/e2e.sh --only newuser-check    # xtask e2e's arguments
#   DATAMAN_LINUX_E2E_TESTS=1 scripts/linux/e2e.sh   # cargo test first, as the workflow does
#
# The build is kept in a docker volume (dataman-linux-e2e-target-<arch>); the logs land in the
# repository's target/e2e, as a run on the host would leave them.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
wf="$root/.github/workflows/linux.yml"
die() { echo "linux/e2e: $*" >&2; exit 1; }
command -v docker > /dev/null || die "no docker"
arch=x86_64
case "$(uname -m)" in arm64|aarch64) arch=aarch64 ;; esac
# The workflow's Rust and packages, read from it so the two stay one.
rust="$(sed -nE 's/^  RUST_TOOLCHAIN: "([^"]+)"/\1/p' "$wf" | head -1)"
deps="$(awk '/^  DEPS: >-/ {f = 1; next} f && /^    / {print; next} f {exit}' "$wf" | xargs)"
[ -n "$rust" ] && [ -n "$deps" ] || die "no RUST_TOOLCHAIN or DEPS in $wf"
img="dataman-linux-e2e:$arch-$rust"
docker build -q -t "$img" --build-arg RUST_TOOLCHAIN="$rust" --build-arg DEPS="$deps" \
  -f "$root/packaging/linux/e2e.Dockerfile" "$root/packaging/linux" > /dev/null
mkdir -p "$root/target/e2e"
tests=""
[ -z "${DATAMAN_LINUX_E2E_TESTS:-}" ] || tests='dbus-run-session -- bash -ec "echo -n ci | gnome-keyring-daemon --unlock --components=secrets > /dev/null; cargo test --release --workspace"'
# The repository at its own path: compose's bind mounts resolve as on the host.
exec docker run --rm --init --network host \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v "$root:$root" -w "$root" \
  -v "dataman-linux-e2e-target-$arch:$root/target" -v "$root/target/e2e:$root/target/e2e" \
  -v "dataman-linux-e2e-cargo-$arch:/usr/local/cargo/registry" -v "dataman-linux-e2e-git-$arch:/usr/local/cargo/git" \
  -e VK_ICD_FILENAMES=/nonexistent.json -e CARGO_TERM_COLOR=always \
  -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}" \
  "$img" bash -ec "
    scripts/duckdb-engine.sh
    ${tests:-true}
    xvfb-run -a -s '-screen 0 1600x1000x24' cargo run --release -p xtask -- e2e ${*:+$(printf '%q ' "$@")}"
