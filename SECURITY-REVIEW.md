# irc2torrent — security & correctness review

Reviewed at `c392f45` (2025-08-26) against the whole of `src/`. No fixes applied yet; this document is
the report, and the fixes follow separately.

> **Reading this later:** paths and line numbers below are as they were at `c392f45` and are not
> updated as the code moves — a report that silently re-points at different code stops being a
> record of what was reviewed. Most notably, `src/platforms/tl.rs` has since been renamed
> `src/platforms/http.rs`, and the URL it built is now assembled from a user-supplied template in
> `src/platforms/url_template.rs`. Finding #1's `sanitize_torrent_filename` survives unchanged;
> the template module applies the same sanitise-then-assert shape to the URL itself, because
> `{name}` and `{id}` are the same untrusted announce-line values one layer up.

The bot's exposure is an IRC channel. Anything reachable from a channel message is reachable by
whoever can post there — for a tracker announce channel, that is a large and untrusted set of people.
That framing drives the severities below.

## Summary

| # | Severity | Issue | Location |
|---|---|---|---|
| 1 | **High** | Arbitrary file write from an announce name (path traversal) | `src/platforms/tl.rs:31-38` |
| 2 | **High** | Remote panic: malformed torrent from tracker | `src/clients/rtorrent.rs:76` |
| 3 | **High** | Remote panic: out-of-range torrent creation date | `src/clients/mod.rs:25` |
| 4 | Medium | Secrets and session cookies written to logs | `src/irc_processor.rs:95`, `src/clients/flood.rs:47`, `src/command_processor.rs:49-50` |
| 5 | Medium | Panic on a command with no params | `src/command_processor.rs:31,47` |
| 6 | Medium | `IrcUserName` security mode can never authorize | `src/auth.rs:82` |
| 7 | Medium | Panic on torrent-file create failure; write errors discarded | `src/platforms/tl.rs:37-38` |
| 8 | Low | Owner's own announcements are silently ignored | `src/auth.rs:47-49,80-88` |
| 9 | Low | Non-constant-time password comparison | `src/auth.rs:68` |
| 10 | Low | NickServ branch is dead code containing two panics | `src/irc_processor.rs:103-108` |
| 11 | Low | `RefCell` borrows held across `.await` | `src/lib.rs:79`, `src/command_processor.rs:110` |
| 12 | Info | Unpinned wildcard dependency; unmaintained `failure` crate | `Cargo.toml` |
| 13 | Info | Dead and half-implemented remote administration | several |

---

## 1. Arbitrary file write from an announce name — **High**

`src/platforms/tl.rs:31-38`

```rust
let torrent_file = name.replace(" ", ".") + ".torrent";
...
let mut out = File::create(self.get_torrent_files_dir().join(torrent_file)).expect("Failed file create");
```

`name` is taken verbatim from the announce regex `Name:'(?P<name>.*)'` in `src/config.rs`, so it is
attacker-controlled text from an IRC message. Two separate problems:

- `PathBuf::join` **replaces** the base path when the argument is absolute. An announce containing
  `Name:'/etc/cron.d/pwn'` writes to `/etc/cron.d/pwn.torrent`, not into the torrent directory.
- `..` segments traverse upward: `Name:'../../../root/.ssh/authorized_keys'` escapes the directory.

The written content is whatever the tracker returned for that torrent ID, so it is not fully
controlled — but the *filename and location* are, and a `.torrent` file is a bencoded blob that many
parsers will happily read. Writing into a directory that is watched, executed, or auto-loaded is the
realistic path to impact. The default `torrent_dir` is `/tmp` (`src/config.rs:130`), and in the
container the process runs as `download`, which bounds but does not remove the risk.

Reachability: `torrent_msg_process` runs whenever a message matches the announce regex and
`authenticate(..., Announcement)` passes, which only checks that the message arrived on a configured
channel — **no sender check**. Anyone who can post in the announce channel can trigger it.

**Fix:** derive a bare filename and never trust it as a path — strip everything up to the last
separator, reject `..`, restrict to a safe character set, cap the length, then canonicalise the
joined path and assert it is still inside `get_torrent_files_dir()` before opening.

## 2. Remote panic: malformed torrent — **High**

`src/clients/rtorrent.rs:76`

```rust
let hasher = Torrent::read_from_bytes(bytes).unwrap();
```

`bytes` is the body the tracker returned. Any response that is not a well-formed torrent — an error
page, a truncated download, a redirect body — panics the process. Since the download is triggered by
an IRC announce, a bogus torrent ID is enough. Note this runs *after* the torrent was already handed
to rTorrent, so the bot dies having half-completed its work.

**Fix:** propagate the parse error; log and continue.

## 3. Remote panic: out-of-range creation date — **High**

`src/clients/mod.rs:25`

```rust
let datetime: DateTime<Local> = DateTime::from(DateTime::from_timestamp(self.creation_date, 0).unwrap());
```

`creation_date` comes from `d.creation_date` over RPC, i.e. from the torrent's own metadata.
`from_timestamp` returns `None` outside roughly ±262,000 years, so a torrent whose `creation date`
field is a large integer panics the bot the moment its list is formatted. This is trivially settable
by whoever produced the torrent.

**Fix:** fall back to the epoch (or render "unknown") instead of unwrapping.

## 4. Secrets and session cookies written to logs — **Medium**

Three separate leaks:

- `src/irc_processor.rs:95` — `info!("{}@{}: {}", nick, channel, inner_message)` logs **every** message
  verbatim. In `SecurityMode::Password`, the authentication message is literally `auth:[<password>]`,
  so the bot's own password lands in the log on every command.
- `src/command_processor.rs:49-50` — logs the command and its raw argument, same exposure.
- `src/clients/flood.rs:47` — `info!("Login response: {:?}", resp)` formats the whole `reqwest::Response`,
  whose `Debug` includes response headers. That is where Flood's `Set-Cookie` session token is. The
  session token for the torrent client goes into the log on every startup.

Logs go to stdout and, when configured, to syslog — neither is a secret store. Config structs holding
`password`, `nick_password` and `rss_key` also all `derive(Debug)` (`src/config.rs`), so any future
`{:?}` on config would leak them too.

**Fix:** redact `auth:[...]` before logging; log only `resp.status()` rather than the response; replace
`derive(Debug)` on secret-bearing structs with a manual impl printing `<redacted>`.

## 5. Panic on a command with no params — **Medium**

`src/command_processor.rs:31,47`

```rust
Regex::new(r"cmd:(?P<command>\w+)(?: params:\((?P<params>.*)\))?")
...
let (command, argument) = (&caps["command"], &caps["params"]);
```

The `params` group is optional, and `is_command()` happily matches a bare `cmd:foo`. Indexing a
capture group that did not participate panics. So `cmd:torrentlist` — one of the bot's own documented
commands, which takes no arguments — kills the process.

Requires passing auth first, so the practical trigger is the owner making a typo rather than an
outsider. Still a crash, and it makes half the command surface unusable.

**Fix:** `caps.name("params").map(|m| m.as_str()).unwrap_or("")`.

## 6. `IrcUserName` security mode can never authorize — **Medium**

`src/auth.rs:82`

```rust
} else if is_owner && nick.eq(channel) {
    return SourceValidityResult::OwnerPrivateMessage;
```

For `Command::PRIVMSG(target, msg)` the first value is the **target**, not the sender. On a private
message to the bot, the target is the *bot's own nick* while `nick` is the sender — so `nick.eq(channel)`
is false for every real PM. `check_security_mode` only accepts `OwnerPrivateMessage` in
`IrcUserName` mode, so that mode returns `NotAuthorized` unconditionally.

Failing closed is the safe direction, but the feature is advertised and does not work.

Underlying this is a systemic confusion between the message *target* and its *source* — see also
finding 10.

**Fix:** pass the sender and the bot's own nick separately; recognise a PM as `target == our_nick`.

## 7. Panic on file-create failure; write errors discarded — **Medium**

`src/platforms/tl.rs:37-38`

```rust
let mut out = File::create(...).expect("Failed file create");
let _ = io::copy(&mut slice, &mut out);
```

`expect` panics on any failure — a read-only volume, a full disk, a permission error, or the traversal
from finding 1 landing somewhere unwritable. The subsequent `io::copy` result is discarded, so a
partial or failed write is reported as success and a truncated `.torrent` is handed onward.

**Fix:** propagate both errors.

## 8. Owner's own announcements are silently ignored — **Low**

`src/auth.rs:47-49` accepts only `SourceValidityResult::AnnounceChannel`, but `validate_source`
returns `OwnerAnnounceChannel` when the sender happens to be the configured owner
(`src/auth.rs:80-82`). If the owner's nick is also the announcer, every announcement is dropped.

**Fix:** accept both variants for announcements.

## 9. Non-constant-time password comparison — **Low**

`src/auth.rs:68` — `if password == p`. `String` equality short-circuits on the first differing byte.
Over IRC, network jitter dwarfs the signal and an attacker gets one guess per message, so this is not
practically exploitable here; it is still free to fix.

**Fix:** `subtle::ConstantTimeEq`, or compare digests.

## 10. NickServ branch is dead code containing two panics — **Low**

`src/irc_processor.rs:103-108`

```rust
} else if channel.eq("NickServ") {
    ...
    let (nick, status) = self.status_response_regex.captures(inner_message)
        .map(|caps| (caps["nick"].to_string(), caps["status"].parse().unwrap())).unwrap();
```

`channel` is the target, so this asks "was this message addressed *to* NickServ" — never true for a
reply *from* NickServ. The branch is therefore unreachable. Were it fixed without also fixing the
unwraps, any NickServ message containing the word `STATUS` but not matching the regex would panic
(`.unwrap()` on `None`), as would a non-numeric status (`parse().unwrap()`).

`update_user_status` and `send_log` also send to `NickServ` unconditionally, and `send_log` sending
arbitrary log text to a service bot looks like a copy-paste error.

**Fix:** match on the sender; handle non-matching messages without unwrapping.

## 11. `RefCell` borrows held across `.await` — **Low**

- `src/lib.rs:79` — `self.irc_processor.borrow_mut().start_listening().await` holds a mutable borrow
  for the entire lifetime of the program. Any other `borrow()` panics. This is almost certainly why
  `periodic_check` (which calls `irc.borrow()`) is commented out at `src/lib.rs:70-74`: enabling it as
  written would panic immediately.
- `src/command_processor.rs:110-112` — the `Ref` from `self.config.borrow()` lives to the end of the
  enclosing `if let`, which contains an `.await`. A `borrow_mut()` during that window panics.

**Fix:** bind the needed values to locals and drop the guard before awaiting; restructure `start`
so the long-lived borrow is not required.

## 12. Dependency hygiene — **Info**

`cargo audit` initially reported **18 vulnerabilities**; it now reports **0**. What moved the needle,
roughly in order of impact:

- **Dropped OpenSSL for rustls.** `openssl`/`openssl-sys` were listed only to force `vendored`; nothing
  in `src/` ever called them. They cost a full from-source OpenSSL build on every clean compile and
  still left RUSTSEC-2025-0022 and RUSTSEC-2025-0004 to track. Three crates default to native-tls
  (`reqwest`, `irc`, `dxr_client`) and each had to opt out explicitly. Note `dxr_client`'s default also
  carried `dxr/i8`, the non-standard `<i8>` XML-RPC type **rTorrent uses for all 64-bit values**; it is
  now enabled explicitly, without which large sizes and timestamps would silently break.
- **Removed `cargo-release` from `[dev-dependencies]`.** It is a binary tool, not a library. Depending
  on it pulled `git2` and a large build-tooling tree into the lockfile, including three advisories.
- **Bumped the vendored `dxr` fork off its old pins.** `reqwest 0.11`/`http 0.2` kept a second, older
  copy of the entire HTTP+TLS stack in the tree — the source of RUSTSEC-2024-0421 (`idna 0.3`) and
  three `rustls-webpki 0.101` advisories. `quick-xml 0.30` → `0.41` cleared RUSTSEC-2026-0194/0195,
  both reachable from parsing an untrusted XML-RPC reply.
- **Removed `tokio-scgi`.** Last released 2021 and pinning `tokio-util` to 0.6, which blocked that
  branch of the tree. It only provided SCGI request framing — a dozen lines, now inline in
  `dxr_client/src/reqwest_support.rs`. It was also listed as a direct dependency of irc2torrent while
  never being referenced from `src/`.
- **Dropped `failure`** (RUSTSEC-2020-0036, RUSTSEC-2019-0036) for `anyhow`, already a dependency.
- **Pinned `pub-sub = "*"`** to `2.0.0`; a wildcard accepts any future release unreviewed.

Three *unmaintained* warnings remain, all transitive and none a vulnerability: `custom_derive` and
`encoding` (via the `irc` crate's default feature set) and `rustls-pemfile` (via rustls). `encoding`
is deliberately kept — dropping it changes how non-UTF-8 IRC messages decode, which would risk
mangling announce lines carrying accented release names.

While replacing the SCGI transport, one more latent panic was removed: the response handler did
`s.split("<?xml").collect::<Vec<_>>()[1]`, which panicked on any reply that was not XML — an SCGI
error response, for instance. It now splits on the header/body boundary and is payload-agnostic.

## 13. Dead and half-implemented remote administration — **Info**

- Six of the eight commands return `"Not implemented yet"`; `remove_watch` exists but nothing calls it.
- `CommandProcessor.authorizer` is constructed and never used — authorization happens in
  `IrcProcessor` instead, so the field is misleading. If a future caller invokes `process_command`
  directly, it will run **unauthenticated**.
- `src/clients/deluge.rs` is empty.
- `src/clients/rtorrent.rs:57` binds `xml_rpc` from a live `.unwrap()` and never uses it.
- Unused `ref u` / `ref p` bindings in `src/auth.rs:60,65`.

Given that this surface is off by default (`is_commands_enabled()`), the recommendation is to keep it
disabled until it is finished, and to have `process_command` enforce authorization itself so that the
guarantee does not depend on the caller.

---

## Note on the panic surface

`src/` contains ~40 `unwrap()`/`expect()`/`panic!` sites. That is tolerable for a bot that only
supervises itself; it becomes significant if this process is made PID 1 of a container, because every
one of them then takes rTorrent and Flood down with it. The findings above cover the ones reachable
from untrusted input; the supervisor work should additionally run the bot in a restartable task so
that a panic degrades to a restart rather than a full-stack outage.
