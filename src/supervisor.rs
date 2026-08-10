//! Container init mode.
//!
//! When `IRC2TORRENT_SUPERVISE` is set, irc2torrent runs as PID 1 and supervises
//! rTorrent and Flood as child processes. This lets the whole stack run on a
//! shell-less hardened base image, with no s6-overlay and no `/bin/sh`.
//!
//! Being PID 1 carries two obligations that a normal process does not have:
//!
//!   * **Signals.** PID 1 gets no default signal dispositions, so without an
//!     explicit handler `SIGTERM` is ignored entirely and `docker stop` waits out
//!     its full timeout before resorting to `SIGKILL`. Handlers are installed for
//!     `SIGTERM` and `SIGINT` and forwarded to the children.
//!   * **Orphan reaping.** Orphaned processes are re-parented to PID 1. If nobody
//!     reaps them they accumulate as zombies until the pid table fills.
//!
//! Reaping is done centrally with `waitpid(-1)` on a dedicated thread rather than
//! through `tokio::process`. Those two cannot safely coexist: a blanket
//! `waitpid(-1)` would consume exit statuses that tokio's process driver is
//! waiting for, so children are spawned with `std::process` and this module owns
//! all reaping.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::Error;
use futures::future::FutureExt;
use log::{error, info, warn};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;

/// First restart delay; doubles up to `RESTART_BACKOFF_MAX`.
const RESTART_BACKOFF_MIN: Duration = Duration::from_secs(1);
const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// How long to wait for the torrent client's control channel -- rTorrent's SCGI
/// socket, or qBittorrent's WebUI port -- before starting Flood anyway.
const CLIENT_WAIT: Duration = Duration::from_secs(60);

/// How long to wait for children to exit after forwarding a termination signal
/// before escalating to SIGKILL.
///
/// **Eight seconds, because `docker stop` defaults to ten.** This was 15, which
/// meant Docker SIGKILLed the whole container -- us and every child -- before
/// the grace period expired, so the escalation never ran and no child ever got
/// a clean exit. rTorrent loses its session state that way; qBittorrent loses
/// its fastresume data, and the next start rechecks the entire library, which on
/// a full seedbox is hours of disk I/O.
///
/// Raise this and `docker stop --time` together if your library needs longer.
fn shutdown_grace() -> Duration {
    Duration::from_secs(env_or("IRC2TORRENT_SHUTDOWN_GRACE", "8").parse().unwrap_or(8))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Read a path from the environment and strip trailing separators.
///
/// Paths supplied by operators routinely carry a trailing slash, and consumers
/// routinely compare them against a normalised form. Flood's `allowedPaths` is
/// the cautionary tale: `FLOOD_OPTION_ALLOWEDPATH=/data/` was compared with
/// `startsWith` against a `realpath`-normalised `/data`, never matched, and
/// every torrent add returned a bare 403 with nothing pointing at the cause.
///
/// Normalise on the way in so nothing downstream has to care. `PathBuf` drops a
/// lone trailing separator on its own, but not a repeated one ("/data//"), so
/// trim explicitly first. A path that is only separators stays "/".
fn env_path(key: &str, default: &str) -> PathBuf {
    let raw = env_or(key, default);
    let trimmed = raw.trim();
    let stripped = trimmed.trim_end_matches('/');

    if stripped.is_empty() {
        // The value was "/" (or only slashes); preserve root rather than "".
        PathBuf::from(if trimmed.is_empty() { default } else { "/" })
    } else {
        PathBuf::from(stripped)
    }
}

/// A supervised child process.
struct Service {
    name: &'static str,
    program: String,
    args: Vec<String>,
    child: Option<Child>,
    backoff: Duration,
    /// When this service is due to be restarted, if it is currently down.
    ///
    /// Restarts are scheduled rather than slept through: awaiting the delay
    /// inside a `tokio::select!` arm runs that arm to completion before the
    /// loop polls anything else, which starved the IRC bot for the whole
    /// backoff. With a 60s backoff the client could not answer server PINGs and
    /// was disconnected every single cycle.
    restart_at: Option<tokio::time::Instant>,
}

impl Service {
    fn new(name: &'static str, program: String, args: Vec<String>) -> Self {
        Self { name, program, args, child: None, backoff: RESTART_BACKOFF_MIN, restart_at: None }
    }

    fn spawn(&mut self) -> Result<u32, Error> {
        // Children inherit our stdout/stderr by default, which does reach
        // `docker logs` -- but unlabelled, so rTorrent's plain text, Flood's
        // JSON and our own timestamped lines interleave with no way to tell
        // them apart. Capture and prefix instead.
        //
        // Set IRC2TORRENT_RAW_CHILD_LOGS=1 to go back to plain inheritance,
        // which keeps Flood's output as machine-parseable JSON for a log
        // shipper.
        let raw = matches!(
            env_or("IRC2TORRENT_RAW_CHILD_LOGS", "0").as_str(),
            "1" | "true" | "yes" | "on"
        );

        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if !raw {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
        }

        let mut child = command
            .spawn()
            .map_err(|e| Error::msg(format!("Could not start {}: {e}", self.name)))?;

        if !raw {
            if let Some(out) = child.stdout.take() {
                relay_output(self.name, out);
            }
            if let Some(err) = child.stderr.take() {
                relay_output(self.name, err);
            }
        }

        let pid = child.id();
        info!("Started {} (pid {pid}): {} {:?}", self.name, self.program, self.args);
        self.child = Some(child);
        Ok(pid)
    }

    fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    fn signal(&self, sig: i32) {
        if let Some(pid) = self.pid() {
            // SAFETY: kill(2) with a pid we spawned; failure is reported, not fatal.
            unsafe {
                libc::kill(pid as libc::pid_t, sig);
            }
        }
    }
}

/// Forward a child's output to our own stderr, tagged with the service name.
///
/// Runs on a plain thread rather than a tokio task because these are blocking
/// `std::process` pipes. It reads bytes rather than `lines()` and converts
/// lossily: `lines()` yields an Err on invalid UTF-8, and bailing out there
/// would stop draining the pipe. Once the 64 KiB pipe buffer filled, the child
/// would then block forever on its next write -- rTorrent silently wedging is a
/// far worse failure than a mangled log line.
fn relay_output<R: std::io::Read + Send + 'static>(name: &'static str, reader: R) {
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(reader);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match std::io::BufRead::read_until(&mut reader, b'\n', &mut buf) {
                Ok(0) => return, // EOF: the child exited
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf);
                    eprintln!("[{name}] {}", line.trim_end_matches(['\n', '\r']));
                }
                Err(_) => return,
            }
        }
    });
}

/// Reap every exited child, including orphans re-parented to us.
///
/// Runs on its own thread because `waitpid` blocks. Results are forwarded to the
/// async side so restart decisions stay in one place.
fn spawn_reaper() -> mpsc::UnboundedReceiver<(i32, i32)> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || loop {
        let mut status: libc::c_int = 0;
        // SAFETY: standard waitpid usage; -1 means "any child".
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid > 0 {
            if tx.send((pid, status)).is_err() {
                return; // supervisor is shutting down
            }
        } else {
            // ECHILD: nothing to wait for yet. Children may still be spawned, so
            // idle briefly rather than exiting the thread.
            std::thread::sleep(Duration::from_millis(200));
        }
    });
    rx
}

fn exit_description(status: i32) -> String {
    if libc::WIFEXITED(status) {
        format!("exit code {}", libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        format!("signal {}", libc::WTERMSIG(status))
    } else {
        format!("status {status}")
    }
}

async fn wait_for_socket(path: &Path) {
    let deadline = tokio::time::Instant::now() + CLIENT_WAIT;
    while tokio::time::Instant::now() < deadline {
        if path.exists() {
            info!("rTorrent socket is up at {}", path.display());
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    warn!(
        "rTorrent socket {} did not appear within {}s; continuing anyway",
        path.display(),
        CLIENT_WAIT.as_secs()
    );
}

/// Wait until something accepts a connection on `addr`.
///
/// The counterpart to `wait_for_socket` for a client whose control channel is
/// TCP. Deliberately an accept and nothing more: qBittorrent answers 403 to an
/// unauthenticated request, so a gate that asked for HTTP 200 would treat a
/// perfectly healthy server as "not ready" and wait out the whole timeout on
/// every single start.
async fn wait_for_tcp(addr: &str, what: &str) {
    let deadline = tokio::time::Instant::now() + CLIENT_WAIT;
    while tokio::time::Instant::now() < deadline {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            info!("{what} is up at {addr}");
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    warn!(
        "{what} did not accept a connection on {addr} within {}s; continuing anyway",
        CLIENT_WAIT.as_secs()
    );
}

/// Put a default config in place on a fresh volume, and only then.
///
/// The runtime has no shell, so this is the only chance to seed one. It must
/// **never** overwrite: qBittorrent rewrites its own ini on every settings
/// change and again on shutdown, so re-seeding each boot would silently discard
/// everything the user set in the WebUI.
fn seed_if_absent(src: &Path, dst: &Path) {
    if dst.exists() || !src.exists() {
        return;
    }
    if let Some(parent) = dst.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!("Could not create {}: {e}", parent.display());
            return;
        }
    }
    match std::fs::copy(src, dst) {
        Ok(_) => info!("Seeded {} from {}.", dst.display(), src.display()),
        Err(e) => warn!("Could not seed {}: {e}", dst.display()),
    }
}

/// Run the IRC bot, restarting it if it panics.
///
/// `Irc2Torrent` holds `Rc`/`RefCell`, so it is `!Send` and cannot be given to
/// `tokio::spawn`. It is instead driven on this task and guarded with
/// `catch_unwind`, so that one of the bot's panic paths degrades to a restart
/// rather than taking rTorrent and Flood down with it.
async fn run_bot_forever() {
    let mut backoff = RESTART_BACKOFF_MIN;
    loop {
        let attempt = std::panic::AssertUnwindSafe(async {
            let mut app = crate::Irc2Torrent::new().await;
            app.start().await;
        })
        .catch_unwind()
        .await;

        match attempt {
            Ok(()) => {
                warn!("IRC bot stopped on its own; restarting in {}s.", backoff.as_secs());
            }
            Err(_) => {
                error!("IRC bot panicked; restarting in {}s.", backoff.as_secs());
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RESTART_BACKOFF_MAX);
    }
}

/// Warn loudly about directories this user cannot write to.
///
/// The common cause is a `/config` volume carried over from an older image.
/// Flood creates its runtime directory with mode 0700, and the previous
/// jesec-based image ran Flood as uid 1001 -- its `adduser download` landed on
/// 1001 because the node base image already occupies 1000. This image runs as
/// 1000, so Flood cannot enter its own old directory and dies with EACCES on
/// users.db, over and over, while rTorrent carries on happily (its files are
/// world-writable if the config sets `system.umask.set = 0000`).
///
/// That combination is very hard to read from a crash loop, so say it plainly.
/// This warns rather than aborts: the container is still useful if only one
/// component's directory is affected.
fn check_writable(dirs: &[String]) {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    for d in dirs {
        let probe = Path::new(d).join(".irc2torrent-write-test");
        match std::fs::File::create(&probe) {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
            }
            Err(e) => {
                error!(
                    "Cannot write to {d} as uid {uid}:{gid} ({e}). If this volume came from the \
                     older image, its files belong to a different user -- fix it with: \
                     docker run --rm -v <volume>:/config alpine chown -R {uid}:{gid} /config"
                );
            }
        }
    }
}

/// Create the directories rTorrent, Flood and irc2torrent expect.
///
/// Upstream rTorrent has no `fs.mkdir` command (that is a jesec-fork extension)
/// and will not create `session.path` itself -- it exits with "Could not create
/// session directory". With no s6 init in this image, the supervisor owns it.
fn prepare_directories(socket: &Path) -> Result<(), Error> {
    let home = env_path("HOME", "/config");
    let base = home.join(".local/share/rtorrent");

    // Flood's documented convention is uppercase, but yargs lowercases the
    // suffix so either spelling reaches it. Check both, or a deployment using
    // one spelling would have Flood serving a directory the supervisor never
    // created. Normalised, so a trailing slash cannot produce a second,
    // near-identical directory alongside the real one.
    let downloads = if std::env::var_os("FLOOD_OPTION_ALLOWEDPATH").is_some() {
        env_path("FLOOD_OPTION_ALLOWEDPATH", "/downloads")
    } else {
        env_path("FLOOD_OPTION_allowedpath", "/downloads")
    };

    // qBittorrent's profile, laid out the way `--profile` expects. Created
    // unconditionally: they cost nothing in the rTorrent image and their absence
    // in the qBittorrent one means the config cannot be seeded at all.
    let qbt_profile = env_path("QBITTORRENT_PROFILE", "/config");

    let mut dirs: Vec<String> = vec![
        base.join(".session"),
        base.join("log"),
        home.join(".local/share/flood"),
        qbt_profile.join("qBittorrent/config"),
        qbt_profile.join("qBittorrent/data"),
        // Config lives directly in the XDG config dir, not a per-app subdirectory:
        // Config::get_full_config_path uses BaseDirs::config_dir().join(filename).
        home.join(".config"),
        downloads,
    ]
    .into_iter()
    .map(|p| p.display().to_string())
    .collect();

    if let Some(parent) = socket.parent() {
        dirs.push(parent.display().to_string());
    }

    for d in &dirs {
        if let Err(e) = std::fs::create_dir_all(d) {
            // A read-only mount for a path we do not strictly need should warn,
            // not abort the whole container.
            warn!("Could not create {d}: {e}");
        }
    }

    check_writable(&dirs);

    // A socket left behind by an unclean shutdown stops rTorrent binding.
    if socket.exists() {
        if let Err(e) = std::fs::remove_file(socket) {
            warn!("Could not remove stale socket {}: {e}", socket.display());
        }
    }
    Ok(())
}

/// Entry point for `IRC2TORRENT_SUPERVISE`.
pub async fn run() -> Result<(), Error> {
    // All normalised: a stray trailing slash on any of these turns an exists()
    // check into a false negative, and the service is then quietly not started.
    let rtorrent_bin = env_path("RTORRENT_BIN", "/usr/local/bin/rtorrent");
    let rtorrent_rc = resolve_rtorrent_rc();
    let rtorrent_socket = env_path("RTORRENT_SOCKET", "/config/.local/share/rtorrent/rtorrent.sock");
    let node_bin = env_path("NODE_BIN", "/usr/bin/node");
    let flood_entry = env_path("FLOOD_ENTRY", "/opt/flood/dist/index.js");

    // The qBittorrent image ships these and no rTorrent; the rTorrent image
    // ships the reverse. Everything below is gated on `exists()`, so one
    // supervisor serves both with no build-time switch.
    let qbt_bin = env_path("QBITTORRENT_BIN", "/usr/bin/qbittorrent-nox");
    let qbt_profile = env_path("QBITTORRENT_PROFILE", "/config");
    let qbt_port = env_or("QBITTORRENT_WEBUI_PORT", "8080");

    warn_if_socket_disagrees_with_rc(&rtorrent_rc, &rtorrent_socket);
    prepare_directories(&rtorrent_socket)?;

    if qbt_bin.exists() {
        seed_if_absent(
            Path::new(IMAGE_QBITTORRENT_CONF),
            &qbt_profile.join("qBittorrent/config/qBittorrent.conf"),
        );
    }

    // Must match session.path.set in rtorrent.rc.
    let rtorrent_base = env_path("HOME", "/config").join(".local/share/rtorrent");
    let session_dir = rtorrent_base.join(".session");
    let log_dir = rtorrent_base.join("log");
    clear_stale_session_lock(&session_dir);
    rotate_rtorrent_log(&log_dir);

    let mut services: Vec<Service> = Vec::new();

    if rtorrent_bin.exists() {
        services.push(Service::new(
            "rtorrent",
            rtorrent_bin.display().to_string(),
            vec![
                "-n".into(),
                "-o".into(),
                format!("import={}", rtorrent_rc.display()),
            ],
        ));
    } else {
        // info, not warn: the qBittorrent image legitimately has no rTorrent,
        // and a warning on every boot of a working container reads as breakage.
        info!("{} not found; not supervising rTorrent.", rtorrent_bin.display());
    }

    if qbt_bin.exists() {
        services.push(Service::new(
            "qbittorrent",
            qbt_bin.display().to_string(),
            vec![
                // Without this it prompts on stdin for the legal notice and
                // exits on EOF. In a container stdin is /dev/null, so the
                // service crash-loops from the first boot with a message that
                // reads like anything but a configuration problem.
                "--confirm-legal-notice".into(),
                format!("--profile={}", qbt_profile.display()),
                // On the command line rather than in the ini: the flag wins over
                // the file, so there is one source of truth for the port the
                // readiness gate below waits on.
                format!("--webui-port={qbt_port}"),
            ],
        ));
    } else {
        info!("{} not found; not supervising qBittorrent.", qbt_bin.display());
    }

    if flood_entry.exists() {
        services.push(Service::new(
            "flood",
            node_bin.display().to_string(),
            vec![
                "--enable-source-maps".into(),
                "--use_strict".into(),
                flood_entry.display().to_string(),
            ],
        ));
    } else {
        info!("{} not found; not supervising Flood.", flood_entry.display());
    }

    // Start the reaper before the first spawn so no exit can be missed.
    let mut reaped = spawn_reaper();

    // Index by pid so a reaped pid can be matched back to its service.
    let mut by_pid: HashMap<i32, usize> = HashMap::new();

    let qbt_addr = format!("127.0.0.1:{qbt_port}");

    for (idx, svc) in services.iter_mut().enumerate() {
        // The torrent client must be up before anything that talks to it:
        // Flood's first system.listMethods probe has to succeed or it settles on
        // the XML-RPC fallback, and the bot's own qBittorrent connect would
        // otherwise race the WebUI bind.
        //
        // Keyed on "is this service the client" rather than on `name == "flood"`
        // as it used to be, so the wait still happens in an image that runs no
        // Flood at all. rTorrent's control channel is a unix socket;
        // qBittorrent's is a TCP port.
        if svc.name != "rtorrent" && svc.name != "qbittorrent" {
            if rtorrent_bin.exists() {
                wait_for_socket(&rtorrent_socket).await;
            } else if qbt_bin.exists() {
                wait_for_tcp(&qbt_addr, "qBittorrent").await;
            }
        }
        match svc.spawn() {
            Ok(pid) => {
                by_pid.insert(pid as i32, idx);
            }
            Err(e) => error!("{e}"),
        }
    }

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    let bot = run_bot_forever();
    tokio::pin!(bot);

    loop {
        // Nothing in this loop may await a long delay inline: every branch body
        // runs to completion before the loop polls the others, so a sleep here
        // stops the IRC bot future being polled at all. Pending restarts are
        // therefore scheduled and waited on as one more select branch.
        let next_restart = services.iter().filter_map(|s| s.restart_at).min();

        tokio::select! {
            // The bot restarts itself, so this only completes if that loop ends.
            _ = &mut bot => {
                warn!("IRC bot supervision ended.");
            }

            _ = sigterm.recv() => {
                info!("SIGTERM received; shutting down.");
                return shutdown(&mut services, &mut reaped).await;
            }

            _ = sigint.recv() => {
                info!("SIGINT received; shutting down.");
                return shutdown(&mut services, &mut reaped).await;
            }

            // Fires when the soonest scheduled restart is due; parks forever
            // when nothing is pending.
            _ = async {
                match next_restart {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let now = tokio::time::Instant::now();
                for idx in 0..services.len() {
                    if services[idx].restart_at.is_some_and(|at| at <= now) {
                        services[idx].restart_at = None;
                        if services[idx].name == "rtorrent" {
                            clear_stale_session_lock(&session_dir);
                            rotate_rtorrent_log(&log_dir);
                        }
                        match services[idx].spawn() {
                            Ok(new_pid) => { by_pid.insert(new_pid as i32, idx); }
                            Err(e) => error!("{e}"),
                        }
                    }
                }
            }

            Some((pid, status)) = reaped.recv() => {
                let Some(&idx) = by_pid.get(&pid) else {
                    // An orphan we inherited. Reaping it is the whole point;
                    // there is nothing else to do.
                    info!("Reaped orphan pid {pid} ({}).", exit_description(status));
                    continue;
                };
                by_pid.remove(&pid);

                let svc = &mut services[idx];
                error!("{} exited ({}); restarting in {}s.", svc.name, exit_description(status), svc.backoff.as_secs());
                // A crash loop is invisible unless someone is reading the logs.
                // Repeats collapse into a count in the digest, so forty restarts
                // arrive as one line rather than forty messages.
                crate::notify::global()
                    .send(crate::notify::Event::ServiceRestarted(svc.name.to_string()));
                svc.child = None;
                svc.restart_at = Some(tokio::time::Instant::now() + svc.backoff);
                svc.backoff = (svc.backoff * 2).min(RESTART_BACKOFF_MAX);
            }
        }
    }
}

/// Remove a session lock left behind by a previous run of *this* container.
///
/// rTorrent stores `hostname:+pid` in `<session>/rtorrent.lock` and only treats
/// it as stale when `kill(pid, 0)` fails. Docker keeps the container hostname
/// across `docker restart`, so a lock written before a restart still matches the
/// hostname while its pid has long since been recycled by some unrelated
/// process. rTorrent then decides the lock is live and refuses to start, for
/// good: "Could not lock session directory, held by: <host>:+<pid>", exit 255,
/// every retry, forever.
///
/// Removing it here is safe precisely because the supervisor owns the lifecycle:
/// this is only called when it has no live rTorrent child. The hostname is still
/// checked, so a genuine lock from a *different* host -- another container
/// sharing the same volume -- is left alone.
fn clear_stale_session_lock(session_dir: &Path) {
    let lock = session_dir.join("rtorrent.lock");

    let Ok(contents) = std::fs::read_to_string(&lock) else {
        return; // absent, or unreadable and not ours to interpret
    };

    // Format is "hostname:+pid"; anything else we leave well alone.
    let Some((host, _)) = contents.trim().rsplit_once(":+") else {
        return;
    };

    let ours = hostname();
    if ours.is_empty() || host != ours {
        warn!(
            "Session lock at {} is held by '{host}', not this host; leaving it",
            lock.display()
        );
        return;
    }

    match std::fs::remove_file(&lock) {
        Ok(()) => info!("Cleared our own stale session lock ({})", contents.trim()),
        Err(e) => warn!("Could not remove {}: {e}", lock.display()),
    }
}

/// The config shipped in the image, holding the issue #443 mitigation.
const IMAGE_RTORRENT_RC: &str = "/etc/rtorrent/rtorrent.rc";

/// The qBittorrent config shipped in the image, seeded onto a fresh volume.
///
/// Unlike the rTorrent rc, which is *imported* on every start and so stays
/// authoritative, this is copied once and then belongs to qBittorrent -- it
/// rewrites the file itself whenever a setting changes.
const IMAGE_QBITTORRENT_CONF: &str = "/etc/qbittorrent/qBittorrent.conf";

/// Decide which rtorrent.rc to load.
///
/// rTorrent is started with `-n`, which disables its own config search
/// entirely (`setup.cc`: `has_flag('n')` returns before looking anywhere), so
/// only the file passed via `-o import=` is read. Dropping a config in one of
/// the conventional places therefore did nothing at all, silently — the
/// defaults kept applying and the user's settings never took effect.
///
/// `RTORRENT_RC` still wins when set. Otherwise the same locations rTorrent
/// would have searched are tried, in its order, before falling back to the
/// image's own config.
fn resolve_rtorrent_rc() -> PathBuf {
    if std::env::var_os("RTORRENT_RC").is_some() {
        let explicit = env_path("RTORRENT_RC", IMAGE_RTORRENT_RC);
        if !explicit.is_file() {
            // `import` is fatal, so rTorrent would exit 255 and be retried
            // forever. Say why now rather than leaving a restart loop to explain
            // itself.
            error!(
                "RTORRENT_RC points at {}, which does not exist; rTorrent will refuse to start",
                explicit.display()
            );
        }
        return explicit;
    }

    let home = env_path("HOME", "/config");
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        candidates.push(PathBuf::from(xdg).join("rtorrent/rtorrent.rc"));
    }
    candidates.push(home.join(".config/rtorrent/rtorrent.rc"));
    candidates.push(home.join(".rtorrent.rc"));

    for candidate in candidates {
        if candidate.is_file() {
            info!("Using rTorrent config {}", candidate.display());
            warn_if_defaults_not_imported(&candidate);
            return candidate;
        }
    }

    PathBuf::from(IMAGE_RTORRENT_RC)
}

/// The socket path an rTorrent config will actually open, following one level of
/// `import` so a user rc that pulls in the image defaults still resolves.
///
/// `None` when there is no scgi line at all, or when the value is a command
/// expression like `(cat,(cfg.basedir),rpc.socket)` that only rTorrent can
/// evaluate -- guessing there would produce a wrong warning, which is worse than
/// none.
fn scgi_socket_in_rc(path: &Path, follow_imports: bool) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut imports: Vec<PathBuf> = Vec::new();

    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim().trim_matches('"'));

        // scgi_local is the deprecated spelling, still accepted by rTorrent.
        if key == "network.scgi.open_local" || key == "scgi_local" {
            return if value.starts_with('(') { None } else { Some(PathBuf::from(value)) };
        }
        if follow_imports && (key == "import" || key == "try_import") {
            imports.push(PathBuf::from(value));
        }
    }

    // One level only: enough to reach the image defaults from a user rc,
    // without needing cycle detection.
    imports.iter().find_map(|p| scgi_socket_in_rc(p, false))
}

/// RTORRENT_SOCKET says where the supervisor *waits*; the socket itself is opened
/// by `network.scgi.open_local` in the rc.
///
/// Setting only the env var therefore looks like it moves the socket but does
/// not, and the failure is mute: the supervisor waits the full SOCKET_WAIT for a
/// path nothing will ever create, then starts Flood against a socket that is not
/// where Flood was told to look.
fn warn_if_socket_disagrees_with_rc(rc: &Path, expected: &Path) {
    let Some(from_rc) = scgi_socket_in_rc(rc, true) else {
        return;
    };
    let from_rc = PathBuf::from(from_rc.to_string_lossy().trim_end_matches('/').to_string());
    if from_rc != expected {
        warn!(
            "RTORRENT_SOCKET is {} but {} opens {}. RTORRENT_SOCKET only tells \
             the supervisor where to wait -- to move the socket itself, change \
             network.scgi.open_local in the rc too.",
            expected.display(),
            rc.display(),
            from_rc.display()
        );
    }
}

/// Point out a user config that does not pull in the image defaults.
///
/// A config written from scratch replaces ours wholesale, which quietly drops
/// `system.files.advise_random` and `pieces.preload.type` -- the entire reason
/// this image exists. That failure is invisible: rTorrent starts happily and
/// simply reads several times more from disk than it uploads.
fn warn_if_defaults_not_imported(path: &Path) {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };

    let imports_defaults = contents.lines().any(|line| {
        let line = line.trim();
        !line.starts_with('#') && line.contains("import") && line.contains(IMAGE_RTORRENT_RC)
    });

    if !imports_defaults {
        warn!(
            "{} does not `import = {IMAGE_RTORRENT_RC}`, so the image defaults \
             (including the issue #443 disk-read mitigation) will not apply",
            path.display()
        );
    }
}

/// Roll rtorrent.log once it grows past `ROTATE_LOG_AT`, keeping one generation.
///
/// The config appends rather than truncates, so the log of whatever killed the
/// previous rTorrent survives a restart -- but rTorrent has no rotation of its
/// own, so something has to bound it. Rolling here, between exits, avoids
/// touching a file rTorrent currently holds open.
fn rotate_rtorrent_log(log_dir: &Path) {
    const ROTATE_LOG_AT: u64 = 16 * 1024 * 1024;

    let log = log_dir.join("rtorrent.log");
    let Ok(meta) = std::fs::metadata(&log) else {
        return;
    };
    if meta.len() < ROTATE_LOG_AT {
        return;
    }

    // A single generation: the previous .1 is discarded. Keeping more would
    // need real rotation, and the recent past is what matters for diagnosis.
    let rolled = log_dir.join("rtorrent.log.1");
    match std::fs::rename(&log, &rolled) {
        Ok(()) => info!(
            "Rolled {} to {} at {} bytes",
            log.display(),
            rolled.display(),
            meta.len()
        ),
        Err(e) => warn!("Could not roll {}: {e}", log.display()),
    }
}

fn hostname() -> String {
    // /etc/hostname is what gethostname() reports in a container, and reading it
    // avoids pulling in a crate just for this.
    std::fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_default()
}

/// Forward the termination signal, wait for children, then escalate.
async fn shutdown(
    services: &mut [Service],
    reaped: &mut mpsc::UnboundedReceiver<(i32, i32)>,
) -> Result<(), Error> {
    let mut outstanding: Vec<u32> = services.iter().filter_map(|s| s.pid()).collect();

    for svc in services.iter() {
        if let Some(pid) = svc.pid() {
            info!("Sending SIGTERM to {} (pid {pid}).", svc.name);
            svc.signal(libc::SIGTERM);
        }
    }

    let deadline = tokio::time::Instant::now() + shutdown_grace();
    while !outstanding.is_empty() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, reaped.recv()).await {
            Ok(Some((pid, status))) => {
                info!("pid {pid} exited ({}).", exit_description(status));
                outstanding.retain(|p| *p as i32 != pid);
            }
            Ok(None) => break,
            Err(_) => break, // grace period elapsed
        }
    }

    for svc in services.iter() {
        if let Some(pid) = svc.pid() {
            if outstanding.contains(&pid) {
                warn!("{} (pid {pid}) did not exit in time; sending SIGKILL.", svc.name);
                svc.signal(libc::SIGKILL);
            }
        }
    }

    info!("Shutdown complete.");
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    // The process environment is global while cargo runs tests on parallel
    // threads, so each test uses its own variable name rather than sharing one.

    /// Guards the failure that motivated `env_path`: an operator-supplied
    /// `FLOOD_OPTION_ALLOWEDPATH=/data/` compared against a realpath-normalised
    /// `/data`, which silently denied every torrent add with a bare 403.
    #[test]
    fn trailing_separators_are_stripped() {
        const KEY: &str = "IRC2TORRENT_TEST_PATH_TRAILING";
        let cases = [
            ("/data/", "/data"),
            ("/data//", "/data"),
            ("/data", "/data"),
            ("  /data/  ", "/data"),
            ("/a/b/c/", "/a/b/c"),
        ];
        for (input, want) in cases {
            std::env::set_var(KEY, input);
            assert_eq!(env_path(KEY, "/unused"), PathBuf::from(want), "input {input:?}");
        }
        std::env::remove_var(KEY);
    }

    #[test]
    fn root_survives_normalisation() {
        const KEY: &str = "IRC2TORRENT_TEST_PATH_ROOT";
        // "/" trims to empty; it must stay root rather than becoming "".
        for input in ["/", "//"] {
            std::env::set_var(KEY, input);
            assert_eq!(env_path(KEY, "/unused"), PathBuf::from("/"), "input {input:?}");
        }
        std::env::remove_var(KEY);
    }

    #[test]
    fn unset_and_empty_fall_back_to_the_default() {
        const KEY: &str = "IRC2TORRENT_TEST_PATH_DEFAULT";
        std::env::remove_var(KEY);
        assert_eq!(env_path(KEY, "/downloads"), PathBuf::from("/downloads"));

        std::env::set_var(KEY, "");
        assert_eq!(env_path(KEY, "/downloads"), PathBuf::from("/downloads"));
        std::env::remove_var(KEY);
    }
}

#[cfg(test)]
mod rc_test {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    /// Each test gets its own directory: cargo runs them on parallel threads.
    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("i2t-rc-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_direct_scgi_line_is_found() {
        let d = tmpdir("direct");
        let rc = write(&d, "rtorrent.rc", "# comment\nnetwork.scgi.open_local  =  /run/x.sock\n");
        assert_eq!(scgi_socket_in_rc(&rc, true), Some(PathBuf::from("/run/x.sock")));
    }

    /// The case this whole check exists for: the user's rc sets no socket of its
    /// own and inherits one from the image defaults it imports.
    #[test]
    fn an_imported_scgi_line_is_found() {
        let d = tmpdir("import");
        let base = write(&d, "base.rc", "network.scgi.open_local = /config/rtorrent.sock\n");
        let user = write(&d, "user.rc", &format!("import = {}\n", base.display()));
        assert_eq!(scgi_socket_in_rc(&user, true), Some(PathBuf::from("/config/rtorrent.sock")));
    }

    #[test]
    fn a_commented_out_line_is_ignored() {
        let d = tmpdir("comment");
        let rc = write(&d, "rtorrent.rc", "#network.scgi.open_local = /nope.sock\n");
        assert_eq!(scgi_socket_in_rc(&rc, true), None);
    }

    /// A computed value is left alone rather than warned about wrongly.
    #[test]
    fn a_command_expression_yields_no_guess() {
        let d = tmpdir("expr");
        let rc = write(&d, "rtorrent.rc", "network.scgi.open_local = (cat,(cfg.basedir),rpc.socket)\n");
        assert_eq!(scgi_socket_in_rc(&rc, true), None);
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        assert_eq!(scgi_socket_in_rc(Path::new("/definitely/not/here.rc"), true), None);
    }

    /// Guards the actual shipped config against drifting away from the
    /// RTORRENT_SOCKET default the image sets and the README documents.
    #[test]
    fn the_shipped_rc_matches_the_documented_default() {
        let rc = Path::new(env!("CARGO_MANIFEST_DIR")).join("docker/rtorrent.rc");
        assert_eq!(
            scgi_socket_in_rc(&rc, false),
            Some(PathBuf::from("/config/.local/share/rtorrent/rtorrent.sock")),
            "docker/rtorrent.rc disagrees with the RTORRENT_SOCKET default in the Dockerfile"
        );
    }

    /// Read one `Key=Value` out of a QSettings-style ini, ignoring sections.
    ///
    /// Enough for the assertions below; qBittorrent's keys are unique across
    /// sections, and the point is to catch a key going missing, not to parse ini.
    fn ini_value(text: &str, key: &str) -> Option<String> {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.starts_with(';') && !l.starts_with('#'))
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('=').map(str::to_string))
    }

    fn shipped_qbittorrent_conf() -> String {
        std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("docker/qBittorrent.conf"),
        )
        .expect("docker/qBittorrent.conf must exist")
    }

    /// The shipped `options.toml` for qBittorrent has empty credentials, which
    /// works *only* because this key turns off authentication for localhost.
    /// Drop it and the bot 403s forever with nothing pointing at the cause.
    #[test]
    fn the_shipped_qbittorrent_conf_bypasses_localhost_auth() {
        let conf = shipped_qbittorrent_conf();
        assert_eq!(
            ini_value(&conf, "WebUI\\LocalHostAuth").as_deref(),
            Some("false"),
            "false here means the bypass is ON -- see the comment in the file"
        );
    }

    /// A 4.x key name would be ignored silently by QSettings, so the save path
    /// would quietly become qBittorrent's own default instead of the volume.
    #[test]
    fn the_shipped_qbittorrent_conf_uses_the_5x_save_path_key() {
        let conf = shipped_qbittorrent_conf();
        assert_eq!(
            ini_value(&conf, "Session\\DefaultSavePath").as_deref(),
            Some("/data"),
            "must match the volume the image exposes"
        );
        // The legal notice must be accepted here as well as on the command
        // line, or a profile seeded from this file still prompts.
        assert_eq!(ini_value(&conf, "Accepted").as_deref(), Some("true"));
    }

    /// Anything with a command-line flag must not also be in the file: the flag
    /// wins, so a second copy can only ever disagree with the one that counts.
    #[test]
    fn the_shipped_qbittorrent_conf_does_not_restate_the_webui_port() {
        let conf = shipped_qbittorrent_conf();
        assert!(
            ini_value(&conf, "WebUI\\Port").is_none(),
            "the port comes from --webui-port, which the readiness gate also reads"
        );
    }

    /// Seeding must be once-only: qBittorrent owns this file after first start,
    /// and overwriting it on every boot would discard the user's settings.
    #[test]
    fn seeding_never_overwrites_an_existing_config() {
        let dir = std::env::temp_dir().join(format!("i2t-seed-{}", std::process::id()));
        let src = dir.join("shipped.conf");
        let dst = dir.join("profile/qBittorrent/config/qBittorrent.conf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&src, "shipped").unwrap();

        // Fresh volume: seeded, including the directories on the way.
        seed_if_absent(&src, &dst);
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "shipped");

        // Now it is qBittorrent's file.
        std::fs::write(&dst, "user's own settings").unwrap();
        seed_if_absent(&src, &dst);
        assert_eq!(
            std::fs::read_to_string(&dst).unwrap(),
            "user's own settings",
            "a second boot must not clobber the running config"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
