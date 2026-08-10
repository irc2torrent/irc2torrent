# syntax=docker/dockerfile:1.7
#############################################################################
# flood_rtorrent_irc2torrent
#
# rakshasa rTorrent + Flood + irc2torrent in one image.
#
# Replaces an earlier build that cloned this repo at build time and layered onto
# jesec/rtorrent-flood:latest -- a distribution dormant since 2023, and missing
# the issue #443 disk-read fix.
#
# Sources arrive as BuildKit named contexts rather than being cloned here, so a
# build uses the working trees as they are and local edits are picked up without
# a push. See docker/build.sh, or the CI workflow which checks the repos out and
# passes them via build-contexts.
#
# Two targets, both supervised by irc2torrent itself (IRC2TORRENT_SUPERVISE):
#   runtime  (default) - shell-less DHI base, smallest attack surface
#   debug              - DHI -dev base, keeps sh + apk for troubleshooting
#############################################################################

ARG ALPINE_IMAGE=dhi.io/alpine-base:3.24-dev
ARG NODE_DEV_IMAGE=dhi.io/node:24-alpine3.24-dev
ARG NODE_RUNTIME_IMAGE=dhi.io/node:24-alpine3.24
ARG RUST_IMAGE=dhi.io/rust:1-alpine3.24-dev

#############################################################################
# libtorrent
#############################################################################
FROM ${ALPINE_IMAGE} AS libtorrent-build

RUN apk add --no-cache \
      build-base automake autoconf libtool pkgconf linux-headers \
      curl-dev openssl-dev zlib-dev ncurses-dev cppunit-dev

WORKDIR /src/libtorrent
COPY --from=libtorrent . .

# Windows checkouts with core.autocrlf=true produce CRLF working trees, which
# autoconf cannot parse ("config.status: error: cannot find input file: '.in'").
# Sources compile fine either way, so only build-system inputs are normalised.
RUN find . \( -name '*.am' -o -name '*.ac' -o -name '*.m4' -o -name '*.in' -o -name '*.sh' \) \
      -type f -exec sed -i 's/\r$//' {} +

RUN autoreconf -fiv \
 && ./configure --prefix=/usr/local --disable-debug \
 && make -j"$(nproc)" \
 && make install \
 && make install DESTDIR=/dist

#############################################################################
# rtorrent
#############################################################################
FROM ${ALPINE_IMAGE} AS rtorrent-build

RUN apk add --no-cache \
      build-base automake autoconf libtool pkgconf linux-headers \
      curl-dev openssl-dev zlib-dev ncurses-dev cppunit-dev

COPY --from=libtorrent-build /dist/ /

WORKDIR /src/rtorrent
COPY --from=rtorrent . .

RUN find . \( -name '*.am' -o -name '*.ac' -o -name '*.m4' -o -name '*.in' -o -name '*.sh' \) \
      -type f -exec sed -i 's/\r$//' {} +

# tinyxml2 is vendored in-tree, so XML-RPC works without the xmlrpc-c dependency.
# Both RPC dialects stay enabled: irc2torrent speaks XML-RPC, Flood prefers JSON-RPC.
RUN autoreconf -fiv \
 && PKG_CONFIG_PATH=/usr/local/lib/pkgconfig \
    ./configure --prefix=/usr/local --disable-debug --with-xmlrpc-tinyxml2 \
 && make -j"$(nproc)" \
 && make install DESTDIR=/dist

#############################################################################
# Flood
#
# The only place npm/pnpm ever runs.
#############################################################################
FROM ${NODE_DEV_IMAGE} AS flood-build

WORKDIR /usr/src/app
COPY --from=flood . .

RUN --mount=type=cache,target=/root/.local/share/pnpm/store \
    npm i -g corepack \
 && corepack enable \
 && corepack install \
 && pnpm install --frozen-lockfile \
 && npm run build

#############################################################################
# irc2torrent
#############################################################################
FROM ${RUST_IMAGE} AS irc2torrent-build

RUN apk add --no-cache build-base perl pkgconf

WORKDIR /src/irc2torrent
COPY Cargo.toml Cargo.lock ./
COPY dxr ./dxr

# Compile the dependencies on their own, against a stub crate.
#
# This is the single biggest cost in CI: 293 crates in release mode, measured at
# 18 minutes of a 20 minute build. It used to be one RUN with
# `--mount=type=cache,target=.../target`, which is precisely the wrong shape
# here -- a BuildKit cache mount lives on the builder and is *not* exported by
# `--cache-to`, so every run began with an empty target directory and rebuilt
# everything no matter how well the registry cache was working.
#
# A plain layer is exported. This one is keyed on Cargo.toml, Cargo.lock and
# dxr, so it survives until the dependency set actually changes and editing
# src/ costs nothing. That rules out cache mounts: the artifacts have to land
# in the layer, which a mount would prevent.
#
# The stub needs both entry points -- this crate is a lib plus a bin, and cargo
# will not build a target whose file is missing.
RUN mkdir -p src \
 && echo 'fn main() {}' > src/main.rs \
 && : > src/lib.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY src ./src
# config.rs does `include_str!("../irc.defaults.toml")`, so the file has to be
# in the build context or this stage fails to compile. Deliberately down here
# and not beside Cargo.toml above: adding it to the dependency layer would
# invalidate that 18-minute build every time the IRC defaults are edited.
COPY irc.defaults.toml ./

# Only this crate is left to compile. `touch` because cargo decides staleness
# from mtimes, and COPY can hand it sources older than the stub just built.
#
# openssl is built vendored (see Cargo.toml), so the binary is self-contained.
RUN touch src/main.rs src/lib.rs \
 && cargo build --release --locked \
 && install -Dm0755 target/release/irc2torrent /dist/usr/local/bin/irc2torrent

#############################################################################
# Runtime userland
#
# Assembles exactly the files the runtime needs into /rootfs, which is then
# copied into the shell-less DHI runtime.
#
# `apk` runs HERE, in a build stage, and its output is never carried across:
# an earlier version of this stage used `apk add --root /rootfs --initdb`, which
# also installs alpine-baselayout and busybox as base dependencies and so put
# /bin/sh and a package database back into the final image -- defeating the
# point of the hardened base. Only individual binaries and the libraries
# `lddtree` says they need are staged now.
#
# Nothing in the stack requires a shell: irc2torrent execve's its children
# directly, and Flood uses spawn/execFile rather than exec. It does invoke the
# `df` and `mediainfo` binaries, and busybox df does not understand
# `--exclude-type`, which is why GNU coreutils is staged.
#############################################################################
FROM ${NODE_DEV_IMAGE} AS userland

ARG PUID=1000
ARG PGID=1000

# Build-stage only: these packages exist so their binaries and .so files can be
# picked out below. The package manager itself never reaches the runtime.
RUN apk add --no-cache \
      coreutils mediainfo ca-certificates tzdata pax-utils \
      libcurl libstdc++ ncurses-libs zlib openssl

# rtorrent/libtorrent/irc2torrent need to be present so their library
# dependencies can be resolved against the same Alpine version.
COPY --from=libtorrent-build /dist/ /
COPY --from=rtorrent-build /dist/ /
COPY --from=irc2torrent-build /dist/ /

# Resolution uses musl's own loader rather than `lddtree`: pax-utils on Alpine
# 3.24 does not ship lddtree, so an earlier version of this silently resolved
# nothing (the failure was inside a pipeline, where `set -e` does not fire) and
# produced an image whose rtorrent died with "symbol not found" for every curses
# call. The assertions at the end exist so that can never ship quietly again.
RUN set -eu; \
    LOADER="/lib/ld-musl-$(uname -m).so.1"; \
    mkdir -p /rootfs; \
    stage() { \
        for f in "$@"; do \
            [ -e "$f" ] || { echo "stage: missing $f" >&2; exit 1; }; \
            install -D "$f" "/rootfs$f"; \
            "$LOADER" --list "$f" \
                | awk '{ for (i = 1; i <= NF; i++) if ($i == "=>") print $(i + 1) }' \
                | while read -r p; do \
                    case "$p" in /*) [ -e "$p" ] && install -D "$p" "/rootfs$p" || true ;; esac; \
                done; \
        done; \
    }; \
    stage /usr/local/bin/rtorrent /usr/local/bin/irc2torrent /bin/coreutils /usr/bin/mediainfo; \
    # coreutils is a multi-call binary; Flood resolves plain "df" through PATH.
    ln -sf coreutils /rootfs/bin/df; \
    # libtorrent's shared objects, which rtorrent links against.
    mkdir -p /rootfs/usr/local/lib; \
    cp -a /usr/local/lib/libtorrent*.so* /rootfs/usr/local/lib/; \
    # TLS trust store for trackers and IRC, plus timezone data.
    install -D /etc/ssl/certs/ca-certificates.crt /rootfs/etc/ssl/certs/ca-certificates.crt; \
    mkdir -p /rootfs/usr/share; cp -a /usr/share/zoneinfo /rootfs/usr/share/; \
    # Volume mount points must exist in the image owned by the runtime user:
    # Docker seeds a named volume from the image directory, ownership included,
    # and the shell-less runtime cannot mkdir/chown for itself.
    mkdir -p /rootfs/config /rootfs/downloads /rootfs/etc/rtorrent; \
    chown -R "${PUID}:${PGID}" /rootfs/config /rootfs/downloads; \
    # Backwards compatibility: the previous image kept the binary at
    # /app/irc2torrent with WORKDIR /app. A symlink instead of a second copy --
    # the runtime has no shell to make one later.
    mkdir -p /rootfs/app; ln -sf /usr/local/bin/irc2torrent /rootfs/app/irc2torrent; \
    # Fail the build rather than ship a shell by accident.
    if [ -e /rootfs/bin/sh ] || [ -e /rootfs/bin/busybox ] || [ -e /rootfs/sbin/apk ]; then \
        echo "staging pulled in a shell or package manager; refusing" >&2; exit 1; \
    fi; \
    # ...and rather than ship binaries whose libraries never got resolved.
    for lib in libcurl.so.4 libncursesw.so.6 libcrypto.so.3 libz.so.1 libstdc++.so.6; do \
        find /rootfs -name "$lib" | grep -q . \
            || { echo "staging did not resolve $lib" >&2; exit 1; }; \
    done; \
    for bin in usr/local/bin/rtorrent usr/local/bin/irc2torrent bin/coreutils; do \
        [ -e "/rootfs/$bin" ] || { echo "staging did not produce $bin" >&2; exit 1; }; \
    done

#############################################################################
# Common assembly
#############################################################################
FROM ${NODE_RUNTIME_IMAGE} AS runtime

LABEL org.opencontainers.image.title="flood_rtorrent_irc2torrent" \
      org.opencontainers.image.description="rakshasa rTorrent 0.16.x + Flood + irc2torrent" \
      org.opencontainers.image.source="https://github.com/irc2torrent/irc2torrent"

# /rootfs is the whole runtime userland: the binaries, exactly the shared
# libraries lddtree resolved for them, the CA bundle and the mount points.
# Deliberately NOT copying the build stages' /dist trees on top -- that would
# re-add the 57 MB rtorrent binary in a second layer and drag in headers,
# static libraries and pkgconfig files that no runtime needs.
COPY --from=userland /rootfs/ /
COPY --from=flood-build /usr/src/app /opt/flood
COPY docker/rtorrent.rc /etc/rtorrent/rtorrent.rc

# The DHI runtime is non-root (uid 1000) and ships no shell, adduser or chown,
# so /config and /downloads are created with the right ownership back in the
# `userland` stage. 1000 matches the previous image's "download" user.
# RTORRENT_RC is deliberately NOT set here. Left unset, the supervisor looks for
# a user config at $HOME/.config/rtorrent/rtorrent.rc or $HOME/.rtorrent.rc
# before falling back to the image's own -- the locations rTorrent would search
# itself, were it not started with -n. Setting it would defeat that.
ENV HOME=/config \
    IRC2TORRENT_SUPERVISE=1 \
    RTORRENT_BIN=/usr/local/bin/rtorrent \
    RTORRENT_SOCKET=/config/.local/share/rtorrent/rtorrent.sock \
    NODE_BIN=/usr/bin/node \
    FLOOD_ENTRY=/opt/flood/dist/index.js \
    FLOOD_OPTION_HOST=:: \
    FLOOD_OPTION_PORT=3000 \
    FLOOD_OPTION_RUNDIR=/config/.local/share/flood \
    FLOOD_OPTION_ALLOWEDPATH=/data

WORKDIR /app
USER 1000:1000

VOLUME ["/config", "/data"]
EXPOSE 3000
EXPOSE 50000
EXPOSE 50000/udp
EXPOSE 50001/udp

ENTRYPOINT ["/usr/local/bin/irc2torrent"]

#############################################################################
# qbt-userland: the same staging technique, for qbittorrent-nox
#
# A separate image, not a variant of the one above: the rTorrent stack is left
# exactly as it is, including the issue #443 work. Reached with
# `--target qbt-runtime`, which prunes the libtorrent and rtorrent stages
# entirely -- so this builds without their named contexts, while still sharing
# the flood-build and irc2torrent-build stages (and therefore their cache).
#############################################################################
FROM ${NODE_DEV_IMAGE} AS qbt-userland

ARG PUID=1000
ARG PGID=1000

# Alpine 3.24 community ships qbittorrent-nox 5.2.1, with Qt6, boost and
# libtorrent-rasterbar versioned consistently against it. Building it from
# source would mean building Qt6, which dwarfs everything else in this file.
#
# icu-data-full rather than icu-data-en: torrent names are aggressively
# non-ASCII, and the difference is tens of megabytes on an image this size.
RUN apk add --no-cache \
      qbittorrent-nox icu-data-full \
      coreutils mediainfo ca-certificates tzdata pax-utils

COPY --from=irc2torrent-build /dist/ /

RUN set -eu; \
    LOADER="/lib/ld-musl-$(uname -m).so.1"; \
    mkdir -p /rootfs; \
    stage() { \
        for f in "$@"; do \
            [ -e "$f" ] || { echo "stage: missing $f" >&2; exit 1; }; \
            install -D "$f" "/rootfs$f"; \
            "$LOADER" --list "$f" \
                | awk '{ for (i = 1; i <= NF; i++) if ($i == "=>") print $(i + 1) }' \
                | while read -r p; do \
                    case "$p" in /*) [ -e "$p" ] && install -D "$p" "/rootfs$p" || true ;; esac; \
                done; \
        done; \
    }; \
    stage /usr/bin/qbittorrent-nox /usr/local/bin/irc2torrent /bin/coreutils /usr/bin/mediainfo; \
    # coreutils is a multi-call binary; Flood resolves plain "df" through PATH.
    ln -sf coreutils /rootfs/bin/df; \
    # Qt opens these with dlopen, so they are not in qbittorrent-nox's
    # DT_NEEDED list and `ld-musl --list` never mentions them. The critical one
    # is tls/libqopensslbackend.so: without it QSslSocket has no backend, so
    # every HTTPS tracker announce fails while the daemon starts happily and
    # seeding works -- the worst possible way for this to break.
    #
    # Staged wholesale rather than by name because the set differs between Qt
    # point releases. Each is run through stage(), so their own dependencies
    # come along too.
    set -- $(find /usr/lib/qt6/plugins -name '*.so' -type f); \
    [ $# -gt 0 ] || { echo "no Qt plugins found to stage" >&2; exit 1; }; \
    stage "$@"; \
    # ICU's data is a plain file the loader knows nothing about, even though
    # Qt6Core links the ICU libraries.
    mkdir -p /rootfs/usr/share; cp -a /usr/share/icu /rootfs/usr/share/; \
    install -D /etc/ssl/certs/ca-certificates.crt /rootfs/etc/ssl/certs/ca-certificates.crt; \
    cp -a /usr/share/zoneinfo /rootfs/usr/share/; \
    # Volume mount points, owned by the runtime user: Docker seeds a named
    # volume from the image directory, ownership included, and the shell-less
    # runtime cannot mkdir or chown for itself.
    mkdir -p /rootfs/config /rootfs/data /rootfs/etc/qbittorrent; \
    chown -R "${PUID}:${PGID}" /rootfs/config /rootfs/data; \
    mkdir -p /rootfs/app; ln -sf /usr/local/bin/irc2torrent /rootfs/app/irc2torrent; \
    # Fail the build rather than ship a shell by accident.
    if [ -e /rootfs/bin/sh ] || [ -e /rootfs/bin/busybox ] || [ -e /rootfs/sbin/apk ]; then \
        echo "staging pulled in a shell or package manager; refusing" >&2; exit 1; \
    fi; \
    # ...and rather than ship a binary whose libraries never got resolved.
    for lib in libQt6Core.so.6 libQt6Network.so.6 libtorrent-rasterbar.so libssl.so.3 libstdc++.so.6; do \
        find /rootfs -name "$lib*" | grep -q . \
            || { echo "staging did not resolve $lib" >&2; exit 1; }; \
    done; \
    # The dlopen'd pieces, named explicitly: the find above could match nothing
    # useful and still be non-empty.
    [ -e /rootfs/usr/lib/qt6/plugins/tls/libqopensslbackend.so ] \
        || { echo "staging did not produce the Qt TLS backend" >&2; exit 1; }; \
    ls /rootfs/usr/share/icu/*/*.dat >/dev/null 2>&1 \
        || { echo "staging did not produce ICU data" >&2; exit 1; }; \
    for bin in usr/bin/qbittorrent-nox usr/local/bin/irc2torrent bin/coreutils; do \
        [ -e "/rootfs/$bin" ] || { echo "staging did not produce $bin" >&2; exit 1; }; \
    done; \
    # The real proof: run it from inside the staged tree. Any gap in the
    # DT_NEEDED closure is an immediate "Error loading shared library" here
    # rather than a container that will not start. musl has no LD_DEBUG, so
    # this stands in for it.
    chroot /rootfs /usr/bin/qbittorrent-nox --version

#############################################################################
# qbt-runtime
#############################################################################
FROM ${NODE_RUNTIME_IMAGE} AS qbt-runtime

LABEL org.opencontainers.image.title="flood_qbittorrent_irc2torrent" \
      org.opencontainers.image.description="qBittorrent 5.x + Flood + irc2torrent" \
      org.opencontainers.image.source="https://github.com/irc2torrent/irc2torrent"

COPY --from=qbt-userland /rootfs/ /
COPY --from=flood-build /usr/src/app /opt/flood
COPY docker/qBittorrent.conf /etc/qbittorrent/qBittorrent.conf

# QT_PLUGIN_PATH is redundant -- the plugins are staged at the same absolute
# paths Qt6Core was compiled to look in -- but it makes an otherwise invisible
# dependency visible to whoever reads this next.
#
# The WebUI port is passed to qbittorrent-nox as --webui-port by the supervisor,
# which reads it from here, so this one variable is the single source of truth
# for the port, the readiness gate and EXPOSE.
ENV HOME=/config \
    IRC2TORRENT_SUPERVISE=1 \
    QT_PLUGIN_PATH=/usr/lib/qt6/plugins \
    IRC2TORRENT_DEFAULT_CLIENT=qBittorrent \
    QBITTORRENT_BIN=/usr/bin/qbittorrent-nox \
    QBITTORRENT_PROFILE=/config \
    QBITTORRENT_WEBUI_PORT=8080 \
    NODE_BIN=/usr/bin/node \
    FLOOD_ENTRY=/opt/flood/dist/index.js \
    FLOOD_OPTION_HOST=:: \
    FLOOD_OPTION_PORT=3000 \
    FLOOD_OPTION_RUNDIR=/config/.local/share/flood \
    FLOOD_OPTION_ALLOWEDPATH=/data

WORKDIR /app
USER 1000:1000

VOLUME ["/config", "/data"]
EXPOSE 3000
EXPOSE 8080
EXPOSE 50000
EXPOSE 50000/udp

ENTRYPOINT ["/usr/local/bin/irc2torrent"]

#############################################################################
# qbt-debug: the same stack on the -dev base, so sh/apk are available
#############################################################################
FROM ${NODE_DEV_IMAGE} AS qbt-debug

COPY --from=irc2torrent-build /dist/ /
COPY --from=flood-build /usr/src/app /opt/flood
COPY docker/qBittorrent.conf /etc/qbittorrent/qBittorrent.conf
COPY --from=irc2torrent-build /dist/usr/local/bin/irc2torrent /app/irc2torrent

RUN apk add --no-cache qbittorrent-nox icu-data-full coreutils mediainfo \
      tzdata ca-certificates \
 && if id -u node >/dev/null 2>&1; then deluser node; fi \
 && if awk -F: '$1=="node"' /etc/group | grep -q .; then delgroup node || true; fi \
 && addgroup -g 1000 download \
 && adduser -D -H -u 1000 -G download -s /sbin/nologin download \
 && mkdir -p /config /data \
 && chown -R download:download /config /data

ENV HOME=/config \
    IRC2TORRENT_SUPERVISE=1 \
    IRC2TORRENT_DEFAULT_CLIENT=qBittorrent \
    QBITTORRENT_BIN=/usr/bin/qbittorrent-nox \
    QBITTORRENT_PROFILE=/config \
    QBITTORRENT_WEBUI_PORT=8080 \
    NODE_BIN=/usr/bin/node \
    FLOOD_ENTRY=/opt/flood/dist/index.js \
    FLOOD_OPTION_HOST=:: \
    FLOOD_OPTION_PORT=3000 \
    FLOOD_OPTION_RUNDIR=/config/.local/share/flood \
    FLOOD_OPTION_ALLOWEDPATH=/data

WORKDIR /app
USER download

VOLUME ["/config", "/data"]
EXPOSE 3000
EXPOSE 8080
EXPOSE 50000
EXPOSE 50000/udp

ENTRYPOINT ["/usr/local/bin/irc2torrent"]

#############################################################################
# debug: same contents on the -dev base, so sh/apk are available
#############################################################################
FROM ${NODE_DEV_IMAGE} AS debug

COPY --from=libtorrent-build /dist/ /
COPY --from=rtorrent-build /dist/ /
COPY --from=irc2torrent-build /dist/ /
COPY --from=flood-build /usr/src/app /opt/flood
COPY docker/rtorrent.rc /etc/rtorrent/rtorrent.rc
COPY --from=irc2torrent-build /dist/usr/local/bin/irc2torrent /app/irc2torrent

RUN apk add --no-cache coreutils mediainfo tzdata ca-certificates \
      libcurl libstdc++ ncurses-libs zlib openssl \
 && if id -u node >/dev/null 2>&1; then deluser node; fi \
 && if awk -F: '$1=="node"' /etc/group | grep -q .; then delgroup node || true; fi \
 && addgroup -g 1000 download \
 && adduser -D -H -u 1000 -G download -s /sbin/nologin download \
 && mkdir -p /config /downloads \
 && chown -R download:download /config /downloads

# RTORRENT_RC is deliberately NOT set here. Left unset, the supervisor looks for
# a user config at $HOME/.config/rtorrent/rtorrent.rc or $HOME/.rtorrent.rc
# before falling back to the image's own -- the locations rTorrent would search
# itself, were it not started with -n. Setting it would defeat that.
ENV HOME=/config \
    IRC2TORRENT_SUPERVISE=1 \
    RTORRENT_BIN=/usr/local/bin/rtorrent \
    RTORRENT_SOCKET=/config/.local/share/rtorrent/rtorrent.sock \
    NODE_BIN=/usr/bin/node \
    FLOOD_ENTRY=/opt/flood/dist/index.js \
    FLOOD_OPTION_HOST=:: \
    FLOOD_OPTION_PORT=3000 \
    FLOOD_OPTION_RUNDIR=/config/.local/share/flood \
    FLOOD_OPTION_ALLOWEDPATH=/downloads

WORKDIR /app
USER download

VOLUME ["/config", "/downloads"]
EXPOSE 3000
EXPOSE 50000
EXPOSE 50000/udp
EXPOSE 50001/udp

ENTRYPOINT ["/usr/local/bin/irc2torrent"]
