# CLAUDE.md

Guidance for Claude Code (claude.ai/code) working in this repository.

## What this is

`irc2torrent` is a Rust IRC announce bot: it watches an announce channel, matches release lines
against user regexes, and adds the matches to rTorrent or qBittorrent. It is also the **build hub**
for a container image bundling the whole stack, so this repo is where the Dockerfile, the runtime
supervisor and the rTorrent config live.

Building the image needs three sibling checkouts, consumed as BuildKit **named contexts** — they are
never cloned by the build:

| Path | `origin` | `upstream` (fetch only) | Version |
|---|---|---|---|
| `../rtorrent/libtorrent/` | `irc2torrent/libtorrent` | `rakshasa/libtorrent` | 0.16.20 |
| `../rtorrent/rtorrent/` | `irc2torrent/rtorrent` | `rakshasa/rtorrent` | 0.16.20 |
| `../rtorrent/flood/` | `irc2torrent/flood` | `jesec/flood` | 4.16.1 |

`docker/build.sh` resolves them as `${WORKSPACE:-$(cd .. && pwd)/rtorrent}`, so the expected layout is:

```
<workspace>/
├── irc2torrent/          <- this repo
└── rtorrent/
    ├── libtorrent/  rtorrent/  flood/
```

Push URLs on `upstream` are set to a bogus string so a mistyped `git push` fails loudly instead of
reaching someone else's repo. `master` tracks `origin/master`, so a bare `git push` goes to the fork.

`dxr/` is **vendored, not a submodule** — a fork of [`ironthree/dxr`](https://github.com/ironthree/dxr)
carrying unix-socket support and a rustls migration. It is consumed as a Cargo path dependency. See
the Vendored code section of `README.md`; keep its `LICENSE-MIT`/`LICENSE-APACHE` intact.

**History**: rtorrent/libtorrent track rakshasa, not the `jesec` fork. That fork has been dormant
since **July 2023** and is missing upstream's entire 2024–2026 thread-safety, socket and DNS rework —
including the issue #443 fix.

## Build & test

**Everything builds in Docker.** `rtorrent` is the client (ncurses UI + RPC + config language);
`libtorrent` is the engine. They must be built and versioned **in lockstep** — rtorrent calls
unstable internal libtorrent APIs, and upstream bumps both to the same version on every release.

```sh
./docker/build.sh                     # -> flood_rtorrent_irc2torrent:dev
TARGET=qbt-runtime ./docker/build.sh  # -> flood_qbittorrent_irc2torrent:dev  (no rtorrent contexts)
TARGET=debug ./docker/build.sh        # keeps sh + apk for troubleshooting
```

Both images come from **one Dockerfile**, selected by `--target`; BuildKit prunes stages outside the
target's graph, which is why `qbt-runtime` needs neither the libtorrent nor the rtorrent tree while
still sharing the `flood-build` and `irc2torrent-build` cache.

For Rust alone, without a full image build:

```sh
docker run --rm --user root -v "$PWD:/src" -w /src <rust-image> cargo test --locked
```

Mount the **whole working copy**, not just `src/`: `the_sample_config_is_valid` reads
`docs/options.sample.toml`, and `get_irc_default_config` does `include_str!("../irc.defaults.toml")`.

**Never run `npm`/`pnpm`/`node` on the host.** Flood is built and tested only inside Docker — a
standing supply-chain constraint, not a convenience.

The base images are `ARG`s defaulting to Docker Hardened Images (`dhi.io/...`), which need
authentication. Override the four ARGs with public equivalents to build without an entitlement; see
README "Base images".

## Architecture

**This repo** — `src/`:
- `lib.rs` wires everything: config → client → platform → IRC/command/torrent processors.
- `config.rs` is one big module: `OptionData` (options.toml), `LoadedOptions` (parsed + compiled
  regexes, and the **only** place validation lives), the file watcher, and live reload.
- `platforms/` is the tracker layer: a `TorrentPlatform` trait, one `HttpTracker` implementation, and
  `url_template.rs`. **There is no tracker-specific code** — a network is an announce regex plus a
  `download_url_template`, both from config.
- `clients/` — rTorrent (XML-RPC over SCGI via dxr), qBittorrent (HTTP), Flood (add-only).
- `supervisor.rs` — the bot runs as **PID 1** in the container (`IRC2TORRENT_SUPERVISE=1`) and
  starts/reaps rTorrent or qBittorrent and Flood itself. There is no s6-overlay.
- `auth.rs`, `notify.rs`, `transports/` — command authorization, notification fan-out, Telegram/Slack.

**libtorrent** — the engine, no UI, no config parsing:
- `src/torrent/` is the public API. `torrent.cc` owns process init and four threads (main, disk, net,
  tracker) — `src/torrent/system/thread.h`.
- `src/torrent/runtime/` — process-wide managers: `NetworkConfig`, `SocketManager`, `MemoryManager`,
  `ProxyManager`, `ClientConfig`.
- `src/data/` — `SocketFile`, `MemoryChunk` (an mmap window), `ChunkList` (mmap cache + sync policy),
  `hash_torrent`/`hash_queue`.
- `src/torrent/data/` — `FileList` → `File` → `FileManager`, a global LRU of open fds bounded by
  `max_open_files`. `FileList::create_chunk()` maps a piece across the files it spans.
- `src/protocol/`, `src/download/`, `src/net/`, `src/tracker/`, `src/dht/`.

**rtorrent** — everything user-facing is a *command*:
- `src/command_*.cc` register named commands (`d.name`, `system.files.advise_random.set`, …) into one
  global `rpc::CommandMap`. `.rtorrent.rc`, ncurses key bindings, XML-RPC and JSON-RPC are all front
  ends resolving names in that map. **Adding a knob = a libtorrent getter/setter plus one `CMD*` line
  in the matching `command_*.cc`.**
- `src/rpc/` — `command_map`, `parse_commands`, `object_storage`, `jsonrpc.cc`,
  `xmlrpc_tinyxml2.cc`, `lua.cc`, `scgi*`. `src/scgi/thread_scgi.cc` runs the SCGI listener.
- `src/core/` — `DownloadList`, `DownloadStore`, `DownloadFactory`, `CurlStack`.

## The issue #443 mitigation

Symptom: rTorrent reads 4–100× more from disk than it uploads, because chunks are served via `mmap()`
and default kernel readahead pulls in far more than the piece being sent.

Fixed upstream in libtorrent `0c1e99a5` (2025-03-28) and `dd66071e` (2025-05-31):
`posix_fadvise(POSIX_FADV_RANDOM)` in `FileManager::open()` plus `madvise(MADV_RANDOM)` in
`FileList::create_chunk_part()`, with a `bool hashing` parameter threaded through so hashing keeps
sequential readahead while seeding does not. rtorrent exposes it at `src/command_local.cc:213-216`.

**Both flags default to `false`** and are undocumented, so upgrading alone fixes nothing. `docker/rtorrent.rc`
turns them on:

```
system.files.advise_random.set = 1     # kernel readahead off for torrent data
pieces.preload.type.set = 1            # rtorrent does its own readahead instead
```

Leave `system.files.advise_random.hashing` at 0 — that split is the entire point of `dd66071e`.

## Flood ↔ rtorrent compatibility

Flood talks to rtorrent over **SCGI** and adapts to what it finds: `clientRequestManager.ts` picks
`methodCallJSON` vs `methodCallXML`, and `clientGatewayService.ts:105` probes `system.listMethods`
over JSON-RPC, clearing `isJSONCapable` on error. `constants/methodCallConfigs/*` are filtered against
`system.listMethods`, so unknown methods are dropped rather than fatal.

Of the ~106 methods Flood calls, three are absent from rakshasa 0.16.20 — all jesec additions, none
blocking:

| Missing | Impact |
|---|---|
| `d.down.sequential{,.set}` | sequential-download toggle silently ineffective; also needs `Download::set_sequential_enabled` in libtorrent |
| `d.timestamp.last_active` | "last active" column/sort unavailable; replicable in `.rtorrent.rc` alone |
| `load.throw` / `load.start_throw` | add-torrent errors not surfaced; Flood falls back to `load.normal`/`load.start` |

`network.http.max_total_connections` and `network.listen.port.range` are *new upstream names* Flood
lists as alternatives to `network.http.max_open` / `network.port_range`, not missing features.

Base64 `data:` URIs work out of the box — `core::is_data_uri()` / `decode_data_uri()` in
`rtorrent/src/core/manager.cc`, wired into `Manager::try_create_download`.

Both RPC layers must stay enabled (they are by default): **irc2torrent needs XML-RPC**, Flood prefers
JSON-RPC. `network.rpc.use_xmlrpc.set` / `network.rpc.use_jsonrpc.set` toggle either at runtime.

irc2torrent reaches rtorrent over a **SCGI unix socket carrying XML-RPC** via vendored `dxr`. Every
method it uses is stock on rakshasa 0.16.20 — `d.multicall2`, `load.raw_start_verbose`, `d.custom.set`,
`system.listMethods`. **No user-defined method is required.** rTorrent resolves a download by hash
case-insensitively, so lowercase hex from `lava_torrent` works as-is.

## Working conventions

- When porting a fix, check whether it needs a matching change on the *other* side of the
  rtorrent/libtorrent pair — new libtorrent capabilities are inert until a command exposes them.
- Upstream lints commit messages: max 3 lines, ≤90 chars per line, no consecutive blank lines
  (`.github/workflows/commit-lint.yml`). Keep fork commits conformant so they can be offered upstream.
- Before concluding a feature is missing from rakshasa, grep the **whole** `src/` tree, not the file
  where the jesec fork happened to put it — the two lay out the same logic in different files.
- Before putting a command in an `.rtorrent.rc`, confirm it is registered. Three traps: `fs.*` is
  jesec-only; `schedule2` / `execute2` / `network.port_range.set` / `network.http.max_open.set` are
  `-D`-only redirects now that `method.use_deprecated` defaults to false; and some commands
  (`encoding.add`, `dht.add_bootstrap`) no longer exist. Macro-made `.set` names will not show up in a
  literal grep — check the `CMD2_VAR_*` macro instead.

### Known build gotchas (all already handled)

- `ACLOCAL_AMFLAGS = -I scripts` in `Makefile.am` duplicates `AC_CONFIG_MACRO_DIRS` and makes
  `libtoolize` fail hard on libtool ≥ 2.5.4 (Alpine 3.22+). Both forks carry the fix on branch
  `fix/aclocal-amflags-libtool-2.5.4` — a good upstream PR.
- A Windows checkout with `core.autocrlf=true` gives CRLF working trees, which autoconf cannot parse
  (`config.status: error: cannot find input file: '.in'`). The libtorrent and rtorrent build stages
  strip CR from `*.am *.ac *.m4 *.in *.sh`. Commits are unaffected — git stores LF.
- rTorrent has no `stderr` log sink, and `/dev/stderr` fails too: it resolves to `/proc/self/fd/2`
  and reopening it after the privilege drop is EACCES. `docker/rtorrent.rc` logs to a file. Its own
  exception output still reaches `docker logs`.
- The `node` base image owns uid/gid 1000, colliding with the usual `PUID`/`PGID`. The runtime is
  shell-less and has no `chown`, so `/config` and `/downloads` are created with the right ownership
  back in the `userland` stage.
- Flood shells out to GNU `df --exclude-type=`, so the runtime stages GNU coreutils and symlinks
  `/bin/df` to it — busybox `df` does not accept that flag.
- Qt loads its TLS backend with `dlopen`, so it appears in no `ldd` listing. Miss it and qBittorrent
  starts, seeds, and silently never announces to an HTTPS tracker. The qbt stage copies the whole
  plugin tree and asserts the TLS backend landed.
- `include_str!("../irc.defaults.toml")` means the Dockerfile must `COPY irc.defaults.toml` — kept
  next to `COPY src`, not with `Cargo.toml`, so editing the IRC defaults does not invalidate the
  ~18-minute dependency layer.
