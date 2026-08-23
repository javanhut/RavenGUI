#!/usr/bin/env bash
# Sync the workspace to the Linux dev box and run a cargo command there.
#
# The compositor cannot build on macOS (libudev/libdrm/libseat/libinput have no
# macOS equivalent), so anything touching smithay compiles on the Linux host.
# Edit here, build there.
#
#   ./scripts/remote.sh check                   # cargo check --workspace
#   ./scripts/remote.sh test -p huginn-core
#   ./scripts/remote.sh run -p huginn-comp      # nested inside the host's session
#   ./scripts/remote.sh sync                    # push files, build nothing
#   ./scripts/remote.sh shell                   # interactive shell, in-tree
#
# Override with RAVEN_REMOTE / RAVEN_REMOTE_DIR / RAVEN_WAYLAND_DISPLAY.
set -euo pipefail

HOST="${RAVEN_REMOTE:-raven}"
REMOTE_DIR="${RAVEN_REMOTE_DIR:-dev/ravengui}"
LOCAL_DIR="$(cd "$(dirname "$0")/.." && pwd)"

EXCLUDES=(target .git .DS_Store)

sync_tree() {
    # rsync is the fast path, but it is not installed on every Arch box and
    # pacman needs a password we do not want in this loop. tar over ssh needs
    # nothing on either end and this tree is small enough that the difference
    # is imperceptible.
    if command -v rsync >/dev/null && ssh "$HOST" 'command -v rsync >/dev/null' 2>/dev/null; then
        local args=()
        for e in "${EXCLUDES[@]}"; do args+=(--exclude "/$e"); done
        rsync -az --delete "${args[@]}" "$LOCAL_DIR/" "$HOST:$REMOTE_DIR/"
    else
        local args=()
        for e in "${EXCLUDES[@]}"; do args+=(--exclude "./$e"); done
        # Deliberately does NOT mirror deletions — tar can only add and
        # overwrite. Delete a file locally and it lingers on the box until you
        # `pacman -S rsync` or clear the tree by hand.
        tar --no-xattrs --no-mac-metadata -czf - "${args[@]}" -C "$LOCAL_DIR" . \
            | ssh "$HOST" "mkdir -p '$REMOTE_DIR' && tar -xzf - -C '$REMOTE_DIR'"
    fi
}

sync_tree

cmd="${1:-check}"
shift || true

# The box runs a COSMIC session on seat0. Pointing a nested run at that socket
# means the compositor opens as a window on the physical monitor, driven from
# here over ssh. The alternative — the udev backend — needs DRM master, which
# logind only grants to the active session on a seat, and an ssh login has no
# seat at all. So udev testing has to happen at the machine, on a TTY.
WL="${RAVEN_WAYLAND_DISPLAY:-wayland-1}"
ENV_PREFIX="export XDG_RUNTIME_DIR=/run/user/\$(id -u) WAYLAND_DISPLAY=$WL;"

case "$cmd" in
    sync)
        echo "synced $LOCAL_DIR -> $HOST:$REMOTE_DIR"
        ;;
    shell)
        exec ssh -t "$HOST" "cd '$REMOTE_DIR' && exec \$SHELL -l"
        ;;
    run)
        ssh "$HOST" "$ENV_PREFIX cd '$REMOTE_DIR' && cargo run --color=always $(printf '%q ' "$@")"
        ;;
    *)
        # --color=always keeps rustc diagnostics readable through ssh, which is
        # not a TTY and would otherwise have every highlight stripped.
        ssh "$HOST" "cd '$REMOTE_DIR' && cargo $cmd --color=always $(printf '%q ' "$@")"
        ;;
esac
