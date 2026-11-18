#!/usr/bin/env bash
# Runs `cargo xtask perf <args>` in the perf dev container (deploy/dev/perf.Dockerfile) with the host's Docker
# socket, for hosts whose Docker runs in a VM, such as Docker Desktop on Windows (docs/testing/perf.md).
#
# Usage, from any directory of a checkout:
#   tools/perf/xtask.sh gate --base main [--candidate HEAD] [--lane 1kb|10kb] [--pairs N] [--rig full|small] [--post]
#   tools/perf/xtask.sh fsync-probe [--rig full|small] [--out FILE]
#
# - The checkout is mounted at /work, and its git directory read-only, so a linked worktree works too.
# - A container cannot see its host's power supply, so on Windows the host's AC-power state is read here and
#   passed in as CRUCIBLE_PERF_AC_POWER. On a Linux host the gate reads it from /sys/class/power_supply.
# - CARGO_BUILD_JOBS, when set, reaches the load generator's build and the image builds.
# - With --post, the host's gh token (gh auth token) signs the commit status in.
# Variables: IMAGE (default crucible-dev-perf:1), TARGET_VOLUME (default crucible-target).
set -euo pipefail
# Git Bash on Windows: pass container paths through unchanged.
export MSYS_NO_PATHCONV=1

host_path() { (cd "$1" && (pwd -W 2>/dev/null || pwd)); }

root=$(host_path "$(dirname "${BASH_SOURCE[0]}")/../..")
IMAGE=${IMAGE:-crucible-dev-perf:1}
TARGET_VOLUME=${TARGET_VOLUME:-crucible-target}

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker build -t "$IMAGE" -f "$root/deploy/dev/perf.Dockerfile" "$root/deploy/dev"
fi

# A linked worktree's git directory lives inside the common one.
common=$(git -C "$root" rev-parse --path-format=absolute --git-common-dir)
gitdir=$(git -C "$root" rev-parse --path-format=absolute --git-dir)

extra=()
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*)
    line=$(powershell.exe -NoProfile -NonInteractive -Command \
      'Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SystemInformation]::PowerStatus.PowerLineStatus' \
      | tr -d '\r')
    case "$line" in
      Online) extra+=(--env CRUCIBLE_PERF_AC_POWER=online) ;;
      Offline) extra+=(--env CRUCIBLE_PERF_AC_POWER=battery) ;;
    esac
    ;;
esac
if [ -n "${CARGO_BUILD_JOBS:-}" ]; then
  extra+=(--env "CARGO_BUILD_JOBS=$CARGO_BUILD_JOBS")
fi
# --post sets a commit status with gh in the container, signed in with the host's gh token. Passing the variable
# by name keeps the token off the command line.
case " $* " in
  *" --post "*)
    if token=$(gh auth token 2>/dev/null); then
      export GH_TOKEN=$token
      extra+=(--env GH_TOKEN)
    fi
    ;;
esac

# --init forwards Ctrl-C to the gate. The label lets the cleanup in docs/testing/perf.md stop a running gate too.
exec docker run --rm --init --label crucible.perf-gate=driver --cpus 3 --memory 2g \
  --volume /var/run/docker.sock:/var/run/docker.sock \
  --volume "$root:/work" --workdir /work \
  --volume "$common:/git:ro" --env "GIT_DIR=/git${gitdir#"$common"}" --env GIT_WORK_TREE=/work \
  --volume "$TARGET_VOLUME:/target" --volume crucible-cargo:/usr/local/cargo/registry \
  ${extra[@]+"${extra[@]}"} "$IMAGE" cargo xtask perf "$@"
