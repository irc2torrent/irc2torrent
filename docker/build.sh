#!/usr/bin/env bash
# Build one of the two images from local working copies.
#
# rTorrent, libtorrent and Flood come from the sibling checkouts via BuildKit
# named contexts. irc2torrent itself is the main context. CI does the same thing
# with actions/checkout + build-push-action's build-contexts, so local and CI
# builds consume identical sources.
#
# Two images, one Dockerfile:
#
#   runtime / debug          rTorrent + Flood + irc2torrent
#   qbt-runtime / qbt-debug  qBittorrent + Flood + irc2torrent
#
# The qBittorrent targets need neither libtorrent nor rTorrent -- BuildKit
# prunes every stage outside the target's graph -- so their contexts are not
# passed and their trees are not required to exist. They do share flood-build
# and irc2torrent-build, so the expensive stages are cached across both images.
#
# Usage:
#   ./build.sh                        # -> flood_rtorrent_irc2torrent:dev
#   TARGET=qbt-runtime ./build.sh     # -> flood_qbittorrent_irc2torrent:dev
#   TARGET=debug ./build.sh           # -> the -dev base, keeps sh + apk
#   TAG=1.2.3 ./build.sh
#   ./build.sh --no-cache             # extra args pass through to buildx
set -euo pipefail

cd "$(dirname "$0")/.."
REPO="$(pwd)"
WS="${WORKSPACE:-$(cd .. && pwd)/rtorrent}"

TAG="${TAG:-dev}"
TARGET="${TARGET:-runtime}"

# Which sources the target actually consumes, and what to call the result.
case "$TARGET" in
    qbt-*)
        CONTEXTS="flood"
        DEFAULT_IMAGE="irc2torrent/flood_qbittorrent_irc2torrent"
        ;;
    *)
        CONTEXTS="libtorrent rtorrent flood"
        DEFAULT_IMAGE="irc2torrent/flood_rtorrent_irc2torrent"
        ;;
esac
IMAGE="${IMAGE:-$DEFAULT_IMAGE}"

context_args=()
for c in $CONTEXTS; do
    [ -d "$WS/$c" ] || { echo "missing source tree: $WS/$c" >&2; exit 1; }
    context_args+=(--build-context "$c=$WS/$c")
done

# Report the ref of every context. rTorrent and libtorrent must carry the
# ACLOCAL_AMFLAGS fix or autoreconf fails outright on libtool >= 2.5.4, and a
# silent mismatch between local and CI is exactly the kind of thing that only
# shows up as a mystifying build error later.
echo "Building ${IMAGE}:${TAG} (target: ${TARGET})"
for c in $CONTEXTS; do
    printf '  %-14s %s @ %s\n' \
        "$c" \
        "$(git -C "$WS/$c" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?')" \
        "$(git -C "$WS/$c" rev-parse --short HEAD 2>/dev/null || echo '?')"
done
printf '  %-14s %s @ %s\n' \
    "irc2torrent" \
    "$(git -C "$REPO" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?')" \
    "$(git -C "$REPO" rev-parse --short HEAD 2>/dev/null || echo '?')"
echo

exec docker buildx build \
    --file "$REPO/Dockerfile" \
    --target "$TARGET" \
    --tag "${IMAGE}:${TAG}" \
    "${context_args[@]}" \
    --build-arg "PUID=${PUID:-1000}" \
    --build-arg "PGID=${PGID:-1000}" \
    --load \
    "$@" \
    "$REPO"
