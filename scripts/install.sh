#!/usr/bin/env bash
# Replace the installed huginn with the one built from this tree.
#
# The desktop that ships on the image is put there by RavenLinux's `gui` stage,
# not by a package: `rvn owns /usr/bin/huginn` reports no owner. So replacing
# these two files corrupts no package database — and equally, nothing but
# `restore` below will ever put them back.
#
# Three things this has to get right:
#
#   * You cannot write over a running executable. The kernel returns ETXTBSY,
#     and huginn is running whenever you are looking at the desktop. So the new
#     binary is written beside the old one and renamed over it: rename swaps the
#     directory entry, the running compositor keeps the inode it started with,
#     and the session carries on undisturbed.
#
#   * Release, not debug. The debug compositor is around 320 MB against 12 MB
#     built with optimisations, and it is slow enough on a real output to feel
#     like a bug in the compositor rather than in the build profile.
#
#   * /usr/sbin and /sbin are symlinks to /usr/bin on this system, so there is
#     one file to replace, not three. Installing to each in turn would just
#     overwrite the same inode and look like it had done more.
#
# Usage:
#   ./scripts/install.sh            build and install
#   ./scripts/install.sh --no-build install what is already built
#   ./scripts/install.sh restore    put the originals back
#   ./scripts/install.sh status     what is installed, and where it came from
#
# PREFIX overrides the install directory (default /usr/bin).
set -euo pipefail
cd "$(dirname "$0")/.."

PREFIX=${PREFIX:-/usr/bin}
BINARIES=(huginn)
PROFILE=release

if [ "$(id -u)" -eq 0 ]; then SUDO=""; else SUDO="sudo"; fi

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }
note() { printf '  %s\n' "$*"; }

# The originals, kept once. A second install must not overwrite this with a
# build of its own, or the thing `restore` returns you to becomes whatever
# happened to be installed last rather than the desktop the image shipped.
backup_of() { printf '%s/%s.orig' "$PREFIX" "$1"; }

check_build_deps() {
    local missing=()
    for lib in libseat libinput; do
        pkg-config --exists "$lib" 2>/dev/null || missing+=("$lib")
    done
    [ ${#missing[@]} -eq 0 ] && return 0
    cat >&2 <<MSG

Cannot build: pkg-config cannot find ${missing[*]}.

The compositor links libseat and libinput, and the runtime libraries alone are
not enough — the build needs their .pc files and their unversioned .so symlinks,
which live in the development packages:

    sudo rvn -i seatd libinput

The libraries themselves are already here (that is why the installed huginn
runs); it is only the build-time half that is missing.
MSG
    exit 1
}

do_build() {
    check_build_deps
    say "Building $PROFILE"
    cargo build --$PROFILE -p huginn-comp
}

do_install() {
    say "Installing to $PREFIX"
    for bin in "${BINARIES[@]}"; do
        local built="target/$PROFILE/$bin"
        if [ ! -x "$built" ]; then
            echo "missing $built — run without --no-build, or build it first" >&2
            exit 1
        fi

        local target="$PREFIX/$bin"
        local backup
        backup=$(backup_of "$bin")
        if [ -e "$target" ] && [ ! -e "$backup" ]; then
            note "keeping the original as $backup"
            $SUDO cp -p "$target" "$backup"
        fi

        # Write beside it, then rename over it. See the header: a running
        # compositor cannot have its own executable truncated underneath it.
        local staged="$target.new.$$"
        $SUDO install -m 0755 "$built" "$staged"
        $SUDO mv -f "$staged" "$target"
        note "$(printf '%-12s %s' "$bin" "$(du -h "$target" | cut -f1)")"
    done
}

do_restore() {
    say "Restoring the originals"
    local restored=0
    for bin in "${BINARIES[@]}"; do
        local backup
        backup=$(backup_of "$bin")
        if [ ! -e "$backup" ]; then
            note "$bin: no $backup — nothing to restore"
            continue
        fi
        local staged="$PREFIX/$bin.new.$$"
        $SUDO cp -p "$backup" "$staged"
        $SUDO mv -f "$staged" "$PREFIX/$bin"
        note "$bin restored"
        restored=1
    done
    [ "$restored" -eq 1 ] && after
}

do_status() {
    say "Installed"
    for bin in "${BINARIES[@]}"; do
        local target="$PREFIX/$bin"
        local backup
        backup=$(backup_of "$bin")
        if [ ! -e "$target" ]; then
            note "$(printf '%-12s %s' "$bin" 'not installed')"
            continue
        fi
        local origin="from the image"
        [ -e "$backup" ] && origin="built from this tree ($backup holds the original)"
        note "$(printf '%-12s %-6s %s  %s' \
            "$bin" "$(du -h "$target" | cut -f1)" \
            "$(date -r "$target" '+%Y-%m-%d %H:%M')" "$origin")"
    done
    local running
    running=$(pgrep -x huginn || true)
    [ -n "$running" ] && note "huginn is running as pid $running — on the binary it started with"
}

after() {
    cat <<'MSG'

  The running compositor is unaffected: it holds the inode it started with, and
  a rename does not reach inside a live process. Log out and back in — or
  Super+Shift+Escape from the session and start it again — to run the new one.
MSG
}

case "${1:-install}" in
    install)    do_build; do_install; after ;;
    --no-build) do_install; after ;;
    restore)    do_restore ;;
    status)     do_status ;;
    *)          sed -n '/^# Usage:/,/^# PREFIX/p' "$0" | sed 's/^# \?//' >&2; exit 2 ;;
esac
