#!/usr/bin/env bash
# Start a full Huginn session on the current TTY: compositor, panel, terminal.
#
# Must be run FROM A TTY (Ctrl+Alt+F2), not over ssh. logind grants DRM master
# only to the active session on a seat, and an ssh login has no seat — the udev
# backend will refuse to start and tell you so.
#
# To leave: Super+Escape quits the compositor. If something goes wrong and the
# screen is unusable, Ctrl+Alt+F1 switches back to your normal session; Huginn
# releases DRM master when it loses the VT.
set -u
cd "$(dirname "$0")/.."

LOG=${HUGINN_LOG:-/tmp/huginn-session.log}
BIN=./target/debug/huginn
PANEL=./target/debug/muninn
TERMINAL=${HUGINN_TERMINAL:-raven-terminal}

[ -x "$BIN" ]   || { echo "build first: cargo build --workspace"; exit 1; }

: >"$LOG"
echo "starting huginn on $(tty); log: $LOG"

RUST_LOG=${RUST_LOG:-huginn=debug,smithay=warn} "$BIN" --backend udev >>"$LOG" 2>&1 &
HUGINN=$!

cleanup() {
    kill "${CHILDREN[@]}" 2>/dev/null
    kill "$HUGINN" 2>/dev/null
}
trap cleanup EXIT INT TERM
CHILDREN=()

# Wait for the socket, but give up if the compositor died — otherwise a startup
# failure just looks like a hang on a blank screen.
for _ in $(seq 1 100); do
    grep -q "huginn is up" "$LOG" 2>/dev/null && break
    if ! kill -0 "$HUGINN" 2>/dev/null; then
        echo "huginn exited during startup:"
        tail -20 "$LOG"
        exit 1
    fi
    sleep 0.1
done

SOCK=$(grep -oP 'WAYLAND_DISPLAY=\K[^ ]+' "$LOG" | head -1)
if [ -z "$SOCK" ]; then
    echo "huginn never reported a socket:"; tail -20 "$LOG"; exit 1
fi
echo "socket: $SOCK"
export WAYLAND_DISPLAY="$SOCK"

if [ -x "$PANEL" ]; then
    "$PANEL" >>"$LOG" 2>&1 &
    CHILDREN+=($!)
fi
# Start the configured terminal, but notice if it dies immediately. A terminal
# built for X11 exits instantly on a Wayland-only compositor, and on a TTY that
# is indistinguishable from "the compositor drew nothing".
start_terminal() {
    local cmd=$1
    command -v "$cmd" >/dev/null || return 1
    "$cmd" >>"$LOG" 2>&1 &
    local pid=$!
    sleep 1
    kill -0 "$pid" 2>/dev/null || return 1
    CHILDREN+=("$pid")
    echo "terminal: $cmd"
    return 0
}

if ! start_terminal "$TERMINAL"; then
    echo "WARNING: $TERMINAL failed to start under Huginn."
    echo "  If it is raven-terminal, it is probably an X11-only build:"
    echo "    cd ~/Development/RavenTerminal && make build BACKEND=wayland"
    echo "  Falling back to kitty so this session is not empty."
    start_terminal kitty || echo "  kitty unavailable too; Super+E still works if you install one."
fi

wait "$HUGINN"
echo "huginn exited. Log: $LOG"
