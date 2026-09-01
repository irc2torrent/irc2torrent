# irc2torrent

Watches an IRC announce channel, matches release lines against your regexes, and hands the
matches to your BitTorrent client.

Announce channels are how a lot of trackers publish new releases in real time. Polling an RSS
feed means waiting out the poll interval; sitting on the channel means acting the moment the
line appears. irc2torrent is the small piece in between — it connects, listens, filters, and
calls your client's RPC.

Written in Rust, cross-platform, and small enough to run alongside the client on a seedbox.
A hardened container image bundling rTorrent and Flood is provided below.

---

## Quick start

### From source

```sh
cargo install --path .
irc2torrent            # writes default configs on first run, then exits
```

Defaults land in `~/.config/`. Edit them, run again.

### Container

**Two images**, same Flood and same bot, differing only in the torrent engine. Both are built on
Docker Hardened Images: the runtime carries no shell, no busybox and no package manager, and runs
as uid 1000 with a read-only root filesystem.

| Image | Engine | Pick it if |
|---|---|---|
| `flood_rtorrent_irc2torrent` | rakshasa rTorrent 0.16.x | you want the [disk-read fix](#the-disk-read-fix) below — it is the reason this fork exists |
| `flood_qbittorrent_irc2torrent` | qBittorrent 5.x | you would rather configure a client through a web UI than an `.rtorrent.rc` |

**rTorrent:**

```sh
docker run -d --name rtorrent \
  --read-only --security-opt no-new-privileges:true \
  --tmpfs /tmp --stop-timeout 30 \
  -v rtorrent-config:/config \
  -v /srv/downloads:/downloads \
  -p 3000:3000 -p 50000:50000 -p 50000:50000/udp -p 50001:50001/udp \
  irc2torrent/flood_rtorrent_irc2torrent:latest
```

Then open <http://localhost:3000>, create the Flood account, and choose
**rTorrent → Unix socket → `/config/.local/share/rtorrent/rtorrent.sock`**.

**qBittorrent:**

```sh
docker run -d --name qbittorrent \
  --read-only --security-opt no-new-privileges:true \
  --tmpfs /tmp --stop-timeout 30 \
  -v qbt-config:/config \
  -v /srv/downloads:/data \
  -p 3000:3000 -p 8080:8080 -p 50000:50000 -p 50000:50000/udp \
  irc2torrent/flood_qbittorrent_irc2torrent:latest
```

Its WebUI is on <http://localhost:8080>, and Flood on <http://localhost:3000> — for Flood, choose
**qBittorrent → `http://127.0.0.1:8080`**. The bot needs no configuration to reach it: the image
ships a config that bypasses authentication for connections from inside the container, and writes
an `options.toml` already pointing at it.

That bypass is keyed on the peer address, so a request arriving through the published port comes
from the Docker bridge rather than 127.0.0.1 and still meets the login form. **Set a WebUI password
anyway** if you expose 8080 beyond the host — qBittorrent prints a temporary one to the log on
first start until you do.

**`--stop-timeout`** on both: Docker defaults to 10 seconds and then SIGKILLs everything, which
costs rTorrent its session state and qBittorrent its fastresume data — and a qBittorrent that lost
its fastresume rechecks the whole library on the next start.

---

## Configuration

Two files, both created with working defaults on first run:
`irc.toml` for the connection, `options.toml` for everything else.

### `options.toml`

```toml
# Releases you want. A line is taken if it matches ANY of these...
regex_for_downloads_match = [
    "Some Release.*2160p.*",
    "Another Release.*S02.*1080p.*WEB.*"
]

# ...and dropped if it matches ANY of these. Reject wins over match.
regex_for_downloads_reject_match = [
    "(?i).*NORDIC.*",
    "(?i).*GERMAN.*",
    "(?i).*SWEDISH.*"
]

# How to pull the release name and torrent id out of an announce line.
# The two named captures are required; everything else about the pattern
# depends on your network's announce format.
regex_for_announce_match = '''.*Name:'(?P<name>.*)' uploaded by.*https://tracker.example.org/torrent/(?P<id>\d+)'''

# The tracker. The key is a label of your choosing and is what gets logged.
# `download_url_template` is where a .torrent is fetched from: copy the URL
# your tracker's download button produces and put {id}, {name}, {file} and
# {key} where the varying parts go. There is deliberately no default.
[platform.YourTracker]
download_url_template = "https://tracker.example.org/rss/download/{id}/{key}/{file}"
rss_key               = "XXXXXXXXXXXXXXXXXXXX"
torrent_dir           = "/downloads/.torrents"

[[clients]]

[clients.rTorrent]
xmlrpc_url = "unix:/config/.local/share/rtorrent/rtorrent.sock"

# Optional: accept commands over IRC. See Security before enabling.
[command_options]
commands_enabled = false

[command_options.security_mode]
IrcUserName = "YourNick"
```

Three notes on the regexes:

- Matching is unanchored, so the leading and trailing `.*` in the filter lists do nothing —
  `"2160p"` and `".*2160p.*"` behave identically. Harmless, just noise.
- Use `(?i)` for case rather than listing variants. One `(?i).*NORDIC.*` covers `NORDiC`,
  `Nordic` and the rest; the sample above collapses five entries into three that way.
- `regex_for_announce_match` is the field you will actually have to write, since every network
  announces differently. Watch the channel by hand for a minute first, or run with
  `RUST_LOG=debug` to see raw lines. `name` and `id` are required and startup fails without
  them, naming the field — that used to be a panic on the first announcement instead.

### Extra fields from the announce line

**Every other named capture becomes an optional field.** Announce lines usually carry more than a
name and an id — a category, an uploader, a freeleech marker — and naming a group is all it takes
to capture it. Nothing about this is tracker-specific, so a network with entirely different
metadata works the same way:

```toml
regex_for_announce_match = '''<(?P<category>[^:]+) :: (?P<subcategory>[^>]+)>\s+Name:'(?P<name>.*)' uploaded by '(?P<uploader>[^']+)'(?P<freeleech> freeleech)?.*/torrent/(?P<id>\d+)'''
```

A group wrapped in `?` is a **marker**: `(?P<freeleech> freeleech)?` captures only when the word is
on the line, so "is this freeleech" is whether the field is present at all — distinct from a group
that matched but captured nothing.

`file` and `key` are reserved for `download_url_template`; a capture named after either is ignored
with a warning rather than an error.

When a single capture holds an unknown number of values, split it:

```toml
[captures.tags]
split = ","
```

Values are trimmed and empty ones dropped. Only worth it when the count is unknown — two fixed
sub-values like `category`/`subcategory` are better written as two groups. Naming a capture here
that the regex does not declare is a startup error, because the alternative is a field that
silently never appears.

Captured fields can be used in [`download_url_template`](#configuration) — a tracker whose download
URL needs an `authkey` from the announce line just names the group and uses `{authkey}`.

### Filtering on those fields

**Rules can be per watch pattern.** An entry in `regex_for_downloads_match` can be a plain string,
as it always was, or a table carrying its own rules:

```toml
regex_for_downloads_match = [
    "Some Release.*1080p.*",
    { match = "Star Trek.*2160p.*", require_fields = ["freeleech"] },
]
```

So "only take the 4K stuff if it's freeleech, anything for the rest" is expressible, which a blanket
setting cannot say. Existing configs are unaffected — a bare string is still a valid entry, and
`cmd:addtowatchlist` still appends one.

The same rules can also be set globally, as a blanket default:

```toml
require_fields = ["freeleech"]        # never take anything without it

[field_filters.category]
matches = "^Movies$"

[field_filters.uploader]
reject_matching = "^(?i)anonymous$"
```

#### How they compose

1. `regex_for_downloads_reject_match` **wins first**, always.
2. The release must match at least one entry, or it is simply not wanted.
3. **A field named by a matching entry is that entry's to decide** — the global rule for that field
   is dropped, not added to. Otherwise a blanket `require_fields` would silently overrule a line
   that deliberately says "this one, freeleech or not".
4. Global rules still apply to fields **no matching entry mentions**.
5. Where several matching entries name the same field, **all of their rules must pass** — overlapping
   patterns compound rather than one quietly winning.

Both `matches` and `reject_matching` test the field's *values*, so a capture with a `split` is tested
per element — one tag out of a list is enough for either to fire.

An absent field answers the two rules oppositely, and neither answer is arbitrary: `matches` rejects
it, since there is nothing to match against; `reject_matching` passes it, since a rule about what a
value looks like cannot fire on a value that is not there. Use `require_fields` for presence.

Naming a field your regex does not declare is a **startup error**, naming the entry — a typo would
otherwise skip every release and look exactly like a dead announce channel.

`cmd:addtorrent` bypasses all of this, as it already did for the name filters: someone who names a
torrent by hand has made the choice the filters exist to make unattended.

### Tagging in qBittorrent

Captured fields can become qBittorrent tags and a category:

```toml
[clients.qBittorrent]
url               = "http://127.0.0.1:8080"
tags_template     = "{category},{uploader}"
category_template = "{category}"
```

qBittorrent's tags are comma-separated, and a capture with `split` expands straight back into a list
— so `tags_template = "{tags}"` over a `split = ","` capture round-trips.

A placeholder whose capture did not fire renders empty and the field is simply not sent, so a
release with no `uploader` gets one fewer tag rather than a blank one. `category_template` overrides
the fixed `category` only when it renders to something, so that stays the fallback.

Values are stripped of CR/LF and capped at 128 characters before being sent — they come off an IRC
announce line, and the request body is multipart, so an unfiltered value would be header injection
into the bot's own request.

Templates are parsed when the client is built, so a malformed one is a startup error rather than a
surprise on the first add — and a placeholder naming a group your regex does not declare is rejected
the same way the filters are.

### Tagging in rTorrent

rTorrent has no category of its own — `d.custom1` is the single label field, and it is what Flood and
ruTorrent both display as tags:

```toml
[clients.rTorrent]
xmlrpc_url    = "unix:/config/.local/share/rtorrent/rtorrent.sock"
tags_template = "{category},{uploader}"
```

**Nothing needs configuring to see these.** Flood asks for `d.custom1` in its ordinary torrent-list
call, and values are encoded exactly as Flood encodes them — trimmed, `encodeURIComponent`d,
deduplicated, comma-joined — so a tag set here is indistinguishable from one you set in the UI. A
comma *inside* a value is encoded rather than read as a separator.

Two things to know: setting this **overwrites** whatever tags the torrent already has, and a failure
to write them is logged but never fails the add — by that point the torrent is loaded and
downloading, so reporting a failed add would be a lie. That is the same treatment the `addtime`
stamp already gets.

And two on the tracker block:

- **Nothing is shipped preconfigured.** `download_url_template`, `server` and `channels` are all
  empty out of the box, and the bot refuses to start until the first two are set. Adding a network
  is configuration, never code.
- `{name}` and `{id}` come verbatim from an IRC message, so every substituted value is
  percent-encoded and the host is re-checked before the request is made. A placeholder in the host
  or port is rejected at startup — otherwise a crafted release name could point the request, and
  your `rss_key`, at someone else's server.

### Which client

**rTorrent** and **qBittorrent** are both fully supported. Flood is present but can only be added
to — it cannot list torrents or report completions, so `cmd:torrentlist` and download-finished
notifications do not work against it.

qBittorrent instead of rTorrent is one block:

```toml
[[clients]]

[clients.qBittorrent]
url      = "http://127.0.0.1:8080"
username = ""      # leave empty where auth is bypassed for localhost
password = ""
```

`save_path` and `category` are optional; empty means whatever qBittorrent is already configured to
use. The bot only logs in when the server asks it to, so an install with authentication bypassed
for localhost needs no credentials at all.

> `[[clients]]` is an array, but **only the first entry is used** — to switch client, reorder them.
> A second entry is ignored, and the log says so. Changing this needs a restart.

In the container, `torrent_dir` must be somewhere writable: the root filesystem is read-only, so
use a path under `/config` or `/downloads`.

**One slash after `unix:`, not two.** `unix://` opens a URL authority, so the first path segment
is parsed as a hostname: `unix://config/.local/…` asks for `/.local/…`, and the only symptom is
a connection error naming a path you never wrote.

The socket must live on a filesystem that supports socket inodes. Local storage — including a
bind mount to a NAS's own disks when Docker runs on the NAS — is fine. A share re-mounted over
**SMB/CIFS cannot host one at all**, and rTorrent will fail to bind at startup rather than
degrade quietly. If that applies, keep `/config` where it is and move just the socket to a local
path (`/run/rtorrent`), changing `network.scgi.open_local`, `RTORRENT_SOCKET` and this
`xmlrpc_url` together.

### What reloads live

Both files are watched. Save, and the change takes effect — no restart:

| | Live | Needs a restart |
|---|---|---|
| `options.toml` | the regexes, the watchlist, `[command_options]`, **all of `[notifications]`** | `platform`, `clients`, `[telegram]`, `[slack]` |
| `irc.toml` | `max_messages_in_burst`, `burst_window_length` | server, port, nickname |

"All of `[notifications]`" includes the backends, not just the switches: add
`[notifications.email]` to a running bot and mail starts working. Only the table you edited is
rebuilt, so retuning `digest_seconds` does not cost an SMTP connection or drop an IRC message
being held for you.

The four exceptions are read once when the objects that use them are built. Each logs a line
saying a restart is needed rather than letting the change look applied. `[telegram]` and `[slack]`
are on that list because they configure the *command* listener too — reloading only the
notification half would leave the two roles on different credentials, so `owner_id` is fixed at
startup along with everything else in the table.

A file that fails to parse, or contains an invalid regex, is rejected: the running config is kept,
the reason is logged, and — if you have notifications on — sent to you.

---

## The disk-read fix

rTorrent serves chunks via `mmap()`, and default kernel readahead pulls in far more than the
piece being uploaded — a seedbox can read 4–100× more from disk than it sends
([rakshasa/rtorrent#443](https://github.com/rakshasa/rtorrent/issues/443)). Upstream merged the
fix, but **both flags default to off**, so the bundled `rtorrent.rc` turns them on:

```
system.files.advise_random.set = 1     # kernel readahead off (posix_fadvise + madvise MADV_RANDOM)
pieces.preload.type.set        = 1     # rTorrent does its own readahead instead
```

`system.files.advise_random.hashing` is deliberately left off — hash checking wants sequential
readahead.

---

## Environment variables

Everything below has a working default baked into the image; you normally need to set **none**
of them.

### Supervisor

Set `IRC2TORRENT_SUPERVISE` and irc2torrent runs as PID 1, starting rTorrent and Flood as child
processes. It installs explicit `SIGTERM`/`SIGINT` handlers (PID 1 gets none by default, so
`docker stop` would otherwise wait out its full timeout), reaps orphaned processes re-parented
to it, and restarts children with capped backoff.

| Variable | Default | Purpose |
|---|---|---|
| `IRC2TORRENT_SUPERVISE` | `1` *(in the image)* | Run as container init. Accepts `1`/`true`/`yes`/`on`. Unset it to run the bot alone. |
| `IRC2TORRENT_RAW_CHILD_LOGS` | `0` | By default child output is captured and prefixed `[rtorrent]` / `[flood]`. Set to `1` for plain inheritance, which keeps Flood's output as machine-parseable JSON for a log shipper. Mutually exclusive with `IRC2TORRENT_SYSLOG_CHILD_LOGS` — see [Remote syslog](#remote-syslog). |
| `RTORRENT_BIN` | `/usr/local/bin/rtorrent` | rTorrent executable. If missing, rTorrent simply is not supervised. |
| `RTORRENT_RC` | `/etc/rtorrent/rtorrent.rc` | Config passed as `-n -o import=…`. |
| `RTORRENT_SOCKET` | `/config/.local/share/rtorrent/rtorrent.sock` | Where the supervisor **waits** for the SCGI socket before starting Flood, so Flood's first `system.listMethods` probe succeeds and it settles on JSON-RPC rather than the XML-RPC fallback. It does **not** move the socket — `network.scgi.open_local` in the rc does. Set only this one and the supervisor warns, then waits out the full 60s for a socket nothing will create. |
| `QBITTORRENT_BIN` | `/usr/bin/qbittorrent-nox` | qBittorrent executable. If missing, qBittorrent is not supervised — which is how one supervisor serves both images with no build-time switch. |
| `QBITTORRENT_PROFILE` | `/config` | Passed as `--profile`. Its config lives at `<profile>/qBittorrent/config/qBittorrent.conf`, seeded from the image on first start and **never overwritten** afterwards — qBittorrent owns that file once it exists. |
| `QBITTORRENT_WEBUI_PORT` | `8080` | Passed as `--webui-port`, and the port the supervisor waits on before starting Flood. One variable, so the gate cannot end up watching a port nothing listens on. |
| `IRC2TORRENT_DEFAULT_CLIENT` | *(unset — rTorrent)* | Which client a freshly written `options.toml` points at. The qBittorrent image sets it to `qBittorrent`. Only affects the file's initial contents. |
| `IRC2TORRENT_SHUTDOWN_GRACE` | `8` | Seconds children get after SIGTERM before SIGKILL. Below `docker stop`'s 10s default on purpose: exceed it and Docker kills everything first, so the escalation never runs and no child exits cleanly. Raise this and `--stop-timeout` together. |
| `NODE_BIN` | `/usr/bin/node` | Node executable used to run Flood. |
| `FLOOD_ENTRY` | `/opt/flood/dist/index.js` | Flood entry point. If missing, Flood is not supervised. |
| `HOME` | `/config` | Root of all persistent state; also where irc2torrent looks for its own config. |

### Remote syslog

Off unless `IRC2TORRENT_SYSLOG` is set. When it is, everything irc2torrent logs goes to the
collector as well as to `docker logs` — no shipper, no sidecar, no file to tail. Lines are
RFC 3164, which rsyslog, syslog-ng, Grafana Alloy and Synology all accept.

```sh
docker run -e IRC2TORRENT_SYSLOG=udp://192.168.1.10:514 …
```

> **Not QNAP's QuLog Center.** Its "Log Receiver" is not a syslog server despite the name — it
> speaks a QNAP-proprietary protocol and only accepts logs from another QNAP NAS running QuLog.
> Nothing sent from here will ever appear there, in any format, over any transport. Use QNAP's
> older **Control Panel → Applications → Syslog Server**, which does take RFC 3164, or a real
> collector. [`docs/loki-stack.compose.yml`](docs/loki-stack.compose.yml) is a ready-to-run
> Loki + Grafana + Prometheus stack that ingests this and keeps 30 days instead of QNAP's 100 MB.

| Variable | Default | Purpose |
|---|---|---|
| `IRC2TORRENT_SYSLOG` | *(unset — off)* | Where to send. `udp://host[:port]`, `tcp://host[:port]`, `unix` for the platform default socket, or `unix:/path`. A bare `host[:port]` means UDP. Port defaults to `514`. IPv6 needs brackets: `udp://[fd00::1]:514`. |
| `IRC2TORRENT_SYSLOG_TAG` | `irc2torrent` | Program name in the header — what most collectors group and filter by. |
| `IRC2TORRENT_SYSLOG_LEVEL` | `info` | Threshold for this sink alone; the terminal keeps its own. `off`/`error`/`warn`/`info`/`debug`/`trace`. |
| `IRC2TORRENT_SYSLOG_FACILITY` | `daemon` | `daemon`, `user`, `local0`–`local7`, … Both `local3` and `LOG_LOCAL3` are accepted. |
| `IRC2TORRENT_SYSLOG_HOSTNAME` | *(system hostname)* | Hostname in the header. Inside a container that defaults to the short container id, which changes on every recreate — set this, or `--hostname`, if you group by host. |
| `IRC2TORRENT_SYSLOG_CHILD_LOGS` | `0` | Also relay rTorrent and Flood output through the sink. See below. |

**UDP is the default deliberately.** The syslog sink is synchronous, and logging calls sit on the
IRC path, so a `tcp://` target that stops answering can stall the bot. Use TCP only if you need
its delivery guarantees and the collector is reliably up.

A target that cannot be opened is **never fatal**: the reason is logged once and the bot keeps
running without the remote sink. Losing the bot because a NAS moved would be the worse outcome.

**Child logs.** By default the sink carries only irc2torrent's own lines — rTorrent and Flood
output goes straight to stderr and never touches the `log` crate, so it stays in `docker logs`
only. `IRC2TORRENT_SYSLOG_CHILD_LOGS=1` routes it through `log` instead, so it reaches the
collector too. One line still, not two, but it picks up irc2torrent's timestamp and level prefix —
which is exactly what destroys Flood's stdout as machine-parseable JSON. Everything lands at
`info`, since neither child's severity convention maps onto `log`'s reliably.

This flag needs the default capture path, so it does nothing when `IRC2TORRENT_RAW_CHILD_LOGS=1`:
that makes children inherit stdio directly, and irc2torrent never sees their output at all. Setting
both is a contradiction — pick one.

Logging is env-only, not an `options.toml` key, and changes need a restart. The logger is built
before the config file is read and `log`'s global logger can only be set once per process, so a
syslog sink could never take part in the live reload the rest of the config gets.

### Flood

Flood reads any of its CLI options from `FLOOD_OPTION_<OPTION>`. The image presets these:

| Variable | Default | Purpose |
|---|---|---|
| `FLOOD_OPTION_HOST` | `::` | Listen address; `::` covers IPv4 and IPv6. |
| `FLOOD_OPTION_PORT` | `3000` | Web UI port. |
| `FLOOD_OPTION_RUNDIR` | `/config/.local/share/flood` | Flood's database and temp files. |
| `FLOOD_OPTION_ALLOWEDPATH` | `/downloads` | Paths Flood may write to. |
| `FLOOD_OPTION_AUTH` | *(unset)* | `default` or `none`. **Set `default` if Flood is reachable beyond localhost.** |

Note the **uppercase** spelling: Flood's own image uses it, and Linux environment variables are
case-sensitive, so mixing `FLOOD_OPTION_HOST` and `FLOOD_OPTION_host` leaves both defined with
no defined precedence.

### Do not set

| Variable | Why |
|---|---|
| `FLOOD_OPTION_RTORRENT` | Makes **Flood** spawn its own rTorrent. The supervisor already does, so you get two instances against one session directory and socket — and Flood's copy does not load `/etc/rtorrent/rtorrent.rc`, so it runs without the #443 fix. Flood also exits when its child dies, causing a restart loop. |
| `NODE_VERSION`, `YARN_VERSION` | Informational values the base image sets for itself. Carrying stale ones over from an older image is misleading. |
| `PATH` | The image's default is already correct; pinning it risks breaking on a future base image change. |

---

## Paths

| Path | Purpose |
|---|---|
| `/config` | All persistent state — rTorrent session, Flood database, irc2torrent config. Back this up. |
| `/config/.config/options.toml` | irc2torrent options: filters, download key, client, notifications. Annotated reference: [`docs/options.sample.toml`](docs/options.sample.toml). |
| `/config/.config/irc.toml` | IRC connection settings. |
| `/config/.local/share/rtorrent/.session` | rTorrent session. |
| `/config/.local/share/rtorrent/log/rtorrent.log` | rTorrent's log. It is a file because rTorrent cannot reopen `/dev/stderr` after dropping privileges; its exceptions still reach `docker logs`. |
| `/downloads` | Torrent data. |

Ports: `3000` Flood, `50000` peer traffic (tcp+udp), `50001/udp` DHT. Change the peer port in
both `docker/rtorrent.rc` and your run command together.

---

## Commands over IRC

Off by default. Set `commands_enabled = true` and a `security_mode`, then send the bot a private
message — see the caveat under [Security](#security) before you do.

Every command has a short form, which is what you will actually type:

| Short | Long | Does |
|---|---|---|
| `at! <link \| id \| announce line>` | `cmd:addtorrent params:(…)` | Add a torrent, ignoring the filters |
| `lt!` | `cmd:torrentlist` | List torrents with size, progress, ratio |
| `lw!` | `cmd:watchlist` | List watch patterns, numbered |
| `aw! <regex>` | `cmd:addtowatchlist params:(…)` | Add a watch pattern |
| `rw! <index>` | `cmd:removewatch params:(…)` | Remove the pattern at that index |
| `tn!` | `cmd:testnotify` | Send a test notification |
| `s!` | `cmd:stop` | Discard replies still queued — notifications are kept |
| `h!` | `cmd:help` | List all of the above, in both forms |

```
at! https://tracker.example.org/torrent/241813706
at! 241813706
aw! (?i)(Show.One|Show.Two).*2160p.*
rw! 2
lt!
```

Short forms are rewritten into the long form before anything else sees them, so both go through
the same parser, the same command table and the same authorization. An unrecognised one — `wow!`
— is not a command at all and falls through to the announce matcher, so this can't swallow
channel traffic.

**`s!` calls off a reply in progress.** A long listing takes about a minute to arrive at the
shipped rate, and the pacer runs on its own task — so the bot is still reading while it drains
and can act on a stop. Everything already queued is discarded on its way out rather than sent,
which clears a long backlog in milliseconds.

It cancels *all* outstanding replies, not just the newest request: with two listings still
draining, "stop" plainly means both. **Notifications are never discarded** — they share the
queue because they share the connection, but being told to stop listing torrents is not a
request to throw away a download-finished alert waiting behind it.

The long form still works everywhere, and takes aliases: `downloadlist` for `torrentlist`,
`listwatch` for `watchlist`, `commands` for `help`.

`command:` works as well as `cmd:`, and both the prefix and the command name are
case-insensitive. The prefix must start the message.

By default every command is gated on the sender being identified to network services — see
[`require_identified`](#security).

**`addtorrent` bypasses `regex_for_downloads_match` and `..._reject_match` deliberately.** Those
lists decide what to take unattended off the announce channel; naming a torrent by hand is
already that decision. The log says so — `Adding '<name>' on request; download filters do not
apply.` — so the two paths are distinguishable.

Given only a link or id, there is no release name to log, so the cached `.torrent` and the log
line use `torrent-<id>`. What the client displays comes from the name inside the torrent itself
and is unaffected.

Both listings answer with **one line per entry**, a header first:

```
cmd:watchlist
  3 patterns:
  [0] Some.Release.*2160p.*
  [1] Another.*S02.*1080p.*WEB.*
  [2] (?i)third

cmd:torrentlist
  2 torrents (1 complete):
  [0] Some.Release.2026.2160p.WEB — 41.20GiB, done, ratio 2.14
  [1] Another.S02E04.1080p.WEB — 3.05GiB, 62%, ratio 0.08
```

IRC caps a message at 512 bytes, so packing a listing into one PRIVMSG lost most of a real
library — hence a message each. The header always states the true total, and anything past the
cap becomes a final `… and N more (not shown)`. Watchlist indices are absolute, so they stay
correct when the listing is cut — otherwise `removewatch` would delete a different pattern than
the one shown.

**Flood limits.** Servers kill clients that send too many messages too quickly, so two settings
bound this:

| Setting | Where | Default | Meaning |
|---|---|---|---|
| `max_reply_lines` | `options.toml`, `[command_options]` | `12` | Lines one listing may span (**IRC only**) |
| `max_messages_in_burst` | `irc.toml` | `5` | Messages released before throttling |
| `burst_window_length` | `irc.toml` | `8` | Seconds that burst is measured over |

**irc2torrent enforces the rate itself.** The `irc` crate carries both keys on its Config and even
provides getters, but version 1.1.0 never reads either one and contains no throttling code at all
— they document behaviour it does not have, which is why a long reply used to leave at full speed
and get the bot killed.

`max_reply_lines` bounds how much a listing *can* be; the burst pair bounds how fast any of it
leaves. Raise them together, and only as far as your network tolerates — a 40-line listing at
5 per 8s takes about a minute to arrive.

**All three are IRC's, and none of them apply to Telegram or Slack.** Both limits exist because a
PRIVMSG is 512 bytes and servers kill clients that send too fast; neither is true there, so a
listing arrives whole and untrimmed however low `max_reply_lines` is set. The only bound that
remains is ten messages per reply — about five hundred torrents — because a hundred messages would
be rate-limited by the platform rather than delivered.

**The burst settings apply the moment you save**, and are the only part of `irc.toml` that does:
server, port and nickname are handed to the client when the connection is built and need a
restart, but these only govern how fast the queue drains. Editing them logs
`Flood limit now N message(s) per Ns; applied immediately.`

`torrentlist` needs rTorrent; Flood exposes progress through its own API, which isn't wired up.

A regex parameter may contain parentheses; the closing delimiter is the **last** `)` in the
message, so `params:((?i)(A|B).*(1080p|2160p))` arrives intact. Put the parameter last: trailing
text containing a `)` would move the delimiter (you'd get an invalid-regex error, not a silent
mis-add).

---

## Telegram

Commands *and* notifications over Telegram, and the better place for both:

```toml
[telegram]
token    = "123456789:AAHdq…"   # from @BotFather
owner_id = 987654321            # from @userinfobot
```

Two lines, and both roles are on. `commands = false` or `notifications = false` turns either off;
`[telegram.events]` filters which events arrive, exactly as for every other target.

Why it is better than IRC for this: **4096 characters per message**, so a whole `lt!` listing
arrives in one piece — no `max_reply_lines`, no pacing, no `… and N more`. And the sender is a
numeric ID issued by Telegram rather than a nickname, so there is nothing to spoof and no WHOIS to
pay for. The bot has no phone number of its own, and receiving works by long polling, so there is
no webhook, no inbound port and nothing to forward on the router.

**[docs/integrations.md](docs/integrations.md) has the step-by-step setup**, including the one trap
worth knowing: Telegram will not let a bot message you until you have messaged it first, and until
you do every notification fails with `chat not found`.

IRC keeps working exactly as before — it is still the announce source, and still accepts commands.

---

## Slack

The same two roles, for a workspace you already live in:

```toml
[slack]
app_token = "xapp-1-A0…"   # App-Level Token, scope connections:write
bot_token = "xoxb-…"       # Bot User OAuth Token, scope chat:write
owner_id  = "U01234567"    # the only person it obeys
```

That is the private setup: the bot opens a direct message with `owner_id` and
lives there. Add one line to put it in a channel instead —

```toml
channel_id = "C01234567"
```

— but know what changes. Only `owner_id` can issue commands either way; the
difference is who reads the answers. In a channel that is everyone in it, and
`lt!` lists your whole library to the room. Worth it when you want a shared
record of what the bot is doing, and not otherwise. The startup log states which
mode it is in.

`commands` / `notifications` / `[slack.events]` work exactly as for Telegram.

Two tokens because Slack separates them by design: the `xapp-` one *only* opens the connection, the
`xoxb-` one *only* posts. Swapping them fails with `not_allowed_token_type`, so the bot warns at
startup if either has the wrong prefix.

Receiving uses **Socket Mode**, which exists for apps with no public URL: the bot dials out over a
WebSocket and Slack pushes events down it. So the same property as Telegram holds — no inbound port,
no certificate, nothing to forward. (Slack's other option, the Events API, would need all three.)

Messages from anyone but `owner_id`, from bots (including its own replies), and from any other
conversation are ignored.

**[docs/integrations.md](docs/integrations.md) has the step-by-step setup** — including an app
manifest you can paste to configure the whole thing in one go, which scopes to grant for which mode,
and the two steps that fail *silently*: subscribe to the wrong event and Slack never delivers
anything, and leave the App Home **Messages Tab** off and a DM setup cannot be sent a command at
all. Neither leaves an error to find.

> Discord would have filled the same role and was planned first. It is blocked where this is
> deployed, so it could be neither used nor tested; shipping an integration nobody involved can
> exercise is worse than not shipping it.

---

## Notifications

Tell you when something happens — a torrent finishes, an add fails, rTorrent starts crash-looping,
the disk fills up. Five ways to receive them, all opt-in: **Telegram** and **Slack** (above),
**email**, **[ntfy](https://ntfy.sh)** (phone push, no account) and an **IRC private message**.

Nothing is enabled until you add a backend table. [`docs/options.sample.toml`](docs/options.sample.toml)
has every option written out with comments; the short version follows.

### Check it works first

```
cmd:testnotify
```

Sends immediately to every configured backend, ignoring every filter. Notification setup fails
*silently* otherwise — a wrong SMTP password produces nothing at all — so run this before trusting
it. If nothing arrives, the log names the reason.

### Email

Usually two lines:

```toml
[notifications.email]
address  = "you@gmail.com"
password = "abcd efgh ijkl mnop"
```

The SMTP host comes from the address's domain, the port and TLS mode from convention (587
STARTTLS, 465 implicit TLS), and both `from` and `to` default to `address`. Known providers:
Gmail, Outlook/Hotmail/Live, Yahoo, Fastmail, iCloud, GMX, Yandex, Zoho, Proton (via Bridge).
Anything else needs an explicit `host`.

**Most providers reject your account password.** You need an app-specific password:

| Provider | Where to get one |
|---|---|
| Gmail | [App passwords](https://myaccount.google.com/apppasswords) — requires 2-Step Verification first |
| Yahoo | [Generate an app password](https://help.yahoo.com/kb/SLN15241.html) |
| iCloud | [App-specific passwords](https://support.apple.com/en-us/102654) |
| Outlook | [App passwords](https://support.microsoft.com/en-us/account-billing/5896ed9b-4263-e681-128a-a6f2979a7944) — needed only with 2FA on |
| Fastmail | [App passwords](https://www.fastmail.help/hc/en-us/articles/360058752854) |
| Proton | Needs [Proton Bridge](https://proton.me/mail/bridge) running; point `host` at it |

If authentication fails against a provider known to require one, the error says so rather than
leaving you with a bare `535`.

To keep the secret out of the config file, use `password_file = "/run/secrets/smtp_password"`
(trailing newline trimmed) or the `IRC2TORRENT_SMTP_PASSWORD` environment variable. First match
wins: `password`, then `password_file`, then the variable.

### ntfy

The lowest-friction option — no account, no API key:

```toml
[notifications.ntfy]
topic = "irc2torrent-8f3a9c1d4b7e"
```

Install the [ntfy app](https://ntfy.sh/#subscribe-phone) (or open `https://ntfy.sh/<topic>`) and
subscribe to the same topic. That's the entire setup.

**Pick something unguessable.** On the public server the topic *is* the access control — anyone
who knows it can read your notifications. A full URL points at a [self-hosted
server](https://docs.ntfy.sh/install/) instead, with an optional `token`:

```toml
[notifications.ntfy]
topic = "https://ntfy.example.org/irc2torrent"
token = "tk_..."
```

### IRC private message

Zero configuration — the bot is already connected and already knows your nick:

```toml
[notifications.irc]
```

Requires `security_mode = IrcUserName`; in `Password` mode there is no nick to message.

The bot checks whether you are actually online (`ISON`, once a minute). A PRIVMSG to an absent
nick is discarded by the server with no error it can act on, so messages sent while you are away
are **held and delivered when you reappear** — up to 20, oldest dropped first.

Holding is for crossing a *disconnect*, not an absence, so held messages also expire:
**`hold_seconds` (default 900)** drops anything that waited longer than fifteen minutes, and the
log says how many went. Without that bound, coming back after a weekend delivered a weekend of
alerts at once, each describing something that had resolved long before you read it. Raise it to
cover a lunch break, or set it to `0` to hold nothing at all.

```toml
[notifications.irc]
hold_seconds = 900
```

Still best-effort: held messages are lost if the bot restarts, and nothing can be sent while the
bot itself is disconnected — which is exactly when `on_failure` matters most. Don't make this your
only backend.

### Which events, and per backend

```toml
[notifications]
on_failure           = true    # add failures, crash loops, rejected config, IRC down
on_torrent_added     = false   # every successful add
on_download_finished = true    # a torrent reaching 100%
on_disk_low          = true    # free space below disk_warn_percent
on_warning           = false   # a release your match list wanted, dropped by your reject list
daily_summary        = false   # one roll-up a day
daily_summary_at     = "09:00" # ...at this local time
on_start             = true    # the bot came up, and IRC came back
```

`daily_summary_at` is 24-hour `HH:MM` in the **bot's local timezone**, so set `TZ` on the container
(`-e TZ=Europe/Oslo`) — the image already carries the zoneinfo, and without it the time is UTC.
Before this existed the roll-up ran on a 24-hour timer started with the process, which put it at
whatever hour the container last came up and moved it again on every restart.

The counters are in memory: a restart resets them, so the summary covers "since the last restart",
not a strict 24 hours. If the bot is down at the appointed time, that day's summary is skipped
rather than sent late.

`on_warning` covers the one filter outcome worth being told about: an announcement that matched
`regex_for_downloads_match` and was then thrown away by `regex_for_downloads_reject_match`. Reject
still wins — this only reports it:

```
Rejected: Some.Release.2160p.GERMAN.WEB (matched '.*2160p.*', rejected by '(?i).*GERMAN.*')
```

Off by default, because a deliberately broad reject list fires on most announcements and running
that way is perfectly reasonable. Turn it on when a release you expected never arrived and you want
to know which of your own patterns ate it. Repeats collapse on the **pair of patterns**, not the
release name, so one over-broad rule reads as a single line with a count rather than one message per
release it dropped. It is logged at `WARN` either way, whether or not you enable the notification.

`on_start` is the greeting: **`irc2torrent 0.13.0 is up (commands, ntfy, telegram)`**, sent to every
configured target when the bot finishes starting, and again as `IRC is back (was down 94s)` when a
reported outage clears. The integration list is the useful part — it is how you confirm a config
change took, and a typo in `[slack]` shows up as Slack simply not being named.

It goes out about **20 seconds** after startup, not on the first digest — waiting five minutes to
learn the bot came up is indistinguishable from it not having come up. A crash loop collapses into
`(x40)` like any other repeat.

Twenty seconds is often too early for a container, whose network is frequently not wired up yet.
That is fine now: **a delivery that fails is held and retried** every 30s for up to six attempts,
so the greeting lands as soon as the network does. This applies to every notification, not just the
greeting — a disk-low warning during a brief outage used to be logged and dropped. The backlog is
five messages per backend, oldest discarded first, and a message that never lands is given up on
with a line saying so.

Any backend can override any of these in its own `events` table. Anything unstated inherits the
global switch — so "phone push for the urgent things, mail for everything" is a few lines:

```toml
[notifications.ntfy.events]
on_torrent_added     = false
on_download_finished = false
daily_summary        = false
```

### Not getting spammed

An announce channel is busy, and this is the difference between a feature you keep and one you
mute after a day:

- **`digest_seconds` (default 300)** — events are buffered and sent as *one* message, with repeats
  collapsed into a count. Forty crash-loop restarts arrive as `rtorrent was restarted (x40)`.
- **`max_per_hour` (default 20)** — a hard ceiling per backend. Nothing is silently lost: the next
  message that goes out says how many were suppressed.
- **`on_torrent_added` is off by default.** It's the setting people enable and immediately regret.

`on_download_finished` works by polling the client every `poll_seconds` (default 120). Torrents
already complete when irc2torrent starts are treated as history, so a restart doesn't announce your
whole library. It needs rTorrent — Flood exposes completion over its own API, which isn't wired up.

---

## Security

irc2torrent parses untrusted input from a network it does not control, so it is built to be
contained: non-root, read-only root filesystem, no shell or package manager in the runtime
image, `no-new-privileges`.

[SECURITY-REVIEW.md](SECURITY-REVIEW.md) has the full audit that preceded 0.2.0 — a
high-severity arbitrary file write reachable from a crafted IRC announce, three remotely
triggerable panics, and dependency work that took `cargo audit` from 18 vulnerabilities to 0.
**All of it is fixed as of 0.2.0**; run that or newer.

**On `commands_enabled`.** This exposes a control interface over IRC, and it is off by default.

A nickname on its own is not a credential: on a network without enforced registration anyone can
take yours the moment you drop off. **`require_identified` (on by default) closes that** — before
running any command the bot asks the network who the sender is, and refuses unless services
report them logged in to an account matching your `IrcUserName`.

```toml
[command_options]
commands_enabled  = true
require_identified = true     # default
```

**Only your own commands cost a WHOIS.** A command-shaped message that could not be authorized
however it resolved — from any nick but the configured one, from anywhere but a private message,
or any message at all while `commands_enabled` is false — is dropped without a lookup and without
a reply. So a stranger typing `h!` in the announce channel costs nothing, learns nothing, and
cannot use the bot to make the server answer questions on their behalf.

Behind that, two bounds apply to what is left: a nick that fails the check three times running is
ignored for ten minutes (told once, when the cooldown starts), and lookups are capped at ten per
minute regardless of nick.

Set `IrcUserName` to your **services account name** — and note that it is matched twice, against
the nick sending the command *and* against the account services report. If your nick and your
account name differ, no command is accepted; use the account name for both.

If the network never answers, the command is refused after 15 seconds — an unanswered identity
check is not permission.

Set `require_identified = false` only if your network has no services. Without it, the guarantee
falls back to "whoever holds this nick", so make sure the network enforces nick registration and
that yours is registered and identified.

This does not replace transport security: IRC private messages are readable by the network
operator, and in `Password` mode the secret travels in the message itself (redacted from the
bot's own logs, but not from the server's).

**On Telegram and Slack.** Neither needs any of that machinery, because the sender's ID is issued
by the platform and travels with the message — it cannot be taken the way a nick can, so there is
nothing to verify and no WHOIS to pay for. What replaces it is the tokens: each is a full
credential for the bot, so `options.toml` should be readable only by the user it runs as. They are
redacted from logs and from every error message, including the ones that carry a URL. Slack
defaults to a direct message for the same reason; setting `channel_id` makes every reply readable
by everyone in that channel, even though only `owner_id` can issue commands.

Found something? See [SECURITY.md](SECURITY.md) for how to report it privately.

---

## Building

```sh
./docker/build.sh                    # -> flood_rtorrent_irc2torrent:dev
TARGET=qbt-runtime ./docker/build.sh # -> flood_qbittorrent_irc2torrent:dev
TAG=0.18.3 ./docker/build.sh
TARGET=debug ./docker/build.sh       # DHI -dev base, keeps sh + apk for troubleshooting
```

rTorrent, libtorrent and Flood come from sibling checkouts via BuildKit named contexts; CI does
the same with `actions/checkout` and `build-contexts`. rTorrent and libtorrent must be built
from a tree carrying the `ACLOCAL_AMFLAGS` fix, without which `autoreconf` fails on
libtool ≥ 2.5.4.

Both images come from **one Dockerfile**, selected by `--target`. BuildKit prunes every stage
outside the target's graph, so `qbt-runtime` never reads the rTorrent or libtorrent contexts and
`build.sh` does not require those trees to exist — while still sharing the `flood-build` and
`irc2torrent-build` stages, and therefore their cache, with the rTorrent image. A second
Dockerfile could not share stages and would rebuild all 293 Rust crates a second time.

The qBittorrent stage has one trap worth knowing if you touch it. The runtime is shell-less, so
binaries and their libraries are staged by resolving `DT_NEEDED` with the musl loader — and Qt
loads its TLS backend and SQL driver with `dlopen`, so they appear in no such listing. Miss them
and the daemon starts happily, seeds happily, and silently never announces to an HTTPS tracker.
The stage copies the whole plugin tree, asserts the TLS backend landed, and finishes with
`chroot /rootfs qbittorrent-nox --version` — which fails the build outright if anything at all is
missing from the closure.

**All Node/pnpm work happens inside the build stage** — never run `npm` against the Flood
checkout on the host.

### Base images

The four base images are `ARG`s, defaulting to Docker Hardened Images:

| `ARG` | Default |
|---|---|
| `ALPINE_IMAGE` | `dhi.io/alpine-base:3.24-dev` |
| `NODE_DEV_IMAGE` | `dhi.io/node:24-alpine3.24-dev` |
| `NODE_RUNTIME_IMAGE` | `dhi.io/node:24-alpine3.24` |
| `RUST_IMAGE` | `dhi.io/rust:1-alpine3.24-dev` |

`dhi.io` requires authentication, so **building without a DHI entitlement means overriding these**
with their public equivalents:

```sh
docker build \
  --build-arg ALPINE_IMAGE=alpine:3.22 \
  --build-arg NODE_DEV_IMAGE=node:24-alpine \
  --build-arg NODE_RUNTIME_IMAGE=node:24-alpine \
  --build-arg RUST_IMAGE=rust:1-alpine \
  ...
```

The result is a working image with an ordinary Alpine userland rather than a hardened one — the
runtime then has a shell and a package manager, which the DHI-based image deliberately does not.

---

## Migrating

### From an `options.toml` written before the tracker was configurable

The tracker used to be compiled in; the download URL is now a template you supply. Existing
configs are otherwise unchanged — add one line to your `[platform.*]` block:

```toml
download_url_template = "https://your.tracker/rss/download/{id}/{key}/{file}"
```

`{file}` and `{key}` are always available — `{key}` is your `rss_key`, and without it the bot
refuses to start and says so, naming the field. **Every other placeholder is one of your captures**:
`{id}` and `{name}` are not special, they are simply the two groups every config declares. A
placeholder naming a group your regex does not declare is a startup error listing what it does
declare, rather than a 404 later.

Two related changes in the same release, both of which only affect *newly generated* configs —
your existing `irc.toml` is untouched:

- The shipped `irc.toml` now has an empty `server` and `channels`, and defaults to `port = 6697`
  with `use_tls = true`. Startup fails with a message if no server is set.
- The table key under `[platform.…]` is now genuinely a label of your choosing. It previously had
  to be one exact word, which is what made following this README produce an `unknown variant`
  error.

### From the jesec-based image

The `/config` layout is unchanged, so existing volumes and an unmodified `options.toml` keep
working. Two things to change:

1. **Remove `FLOOD_OPTION_RTORRENT`** — see above.
2. Drop `NODE_VERSION`, `YARN_VERSION` and `PATH` overrides.

---

## Scope

This ships no tracker configuration and no announce patterns for any particular network.
What you connect to, and whether you are entitled to what you download, is yours to sort out.

## Status

Early. The IRC and filter layers work and are in daily use. The tracker layer is driven entirely
from config — an announce regex and a download-URL template — so a new network needs no code, but
the torrent-client abstraction still has exactly one HTTP implementation behind it. Issues and PRs
welcome, particularly announce patterns for networks not yet covered.

## License

Licensed under [MIT](LICENSE).

### Vendored code

`dxr/` is a vendored copy of [dxr](https://github.com/ironthree/dxr) by Fabio Valentini — the
XML-RPC library this bot talks to rTorrent with. It is dual-licensed MIT / Apache-2.0; both license
texts are kept in `dxr/`, unmodified, alongside upstream's own `README.md` and `CHANGELOG.md`.

It is vendored rather than depended on from crates.io because it carries local changes, taken from
upstream at commit `9b4f7c5`:

- Unix-socket support for `dxr_client` and its `reqwest` transport, which is how rTorrent's SCGI
  socket is reached at all
- A refactor away from `async-trait`
- A move to `rustls`, dropping `tokio-scgi` and clearing the RUSTSEC advisories it carried
- Removal of imports left unused by that rewrite
- A deserialization fix for `quick-xml` 0.41

All credit for dxr belongs upstream; the bugs in the local changes do not.
