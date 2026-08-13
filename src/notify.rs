//! Event notifications: email, ntfy, and IRC private message.
//!
//! Three properties shape this module, in order of how much they matter:
//!
//!  * **It cannot wedge the bot.** Notifying is fire-and-forget through a
//!    bounded channel. An SMTP connect to a wrong host blocks for minutes, and a
//!    long await on the bot's own task has already caused an outage here once
//!    (a `sleep` inside a `tokio::select!` arm starved the IRC client for a full
//!    60s backoff). Nothing on a hot path ever awaits a backend.
//!  * **It must not spam.** An announce channel is busy. Events are buffered
//!    into a digest window, repeats of the same kind are collapsed into a count,
//!    and there is a hard per-hour ceiling whose overflow is *reported* rather
//!    than silently dropped.
//!  * **Each backend switches itself on.** A backend with no usable config is
//!    simply absent, so an untouched `[notifications]` section sends nothing.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Error;
use log::{error, info, warn};
use tokio::sync::mpsc;

use crate::config::config::{EmailOptions, EventFilter, NotificationOptions, NtfyOptions};

/// Queue depth before events are dropped.
///
/// Bounded on purpose: the alternative is an unbounded queue that grows without
/// limit when a backend is wedged. Drops are counted and reported.
const QUEUE_DEPTH: usize = 512;
/// How long a backend gets before its send is abandoned.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Something worth telling the operator about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Event {
    TorrentAdded(String),
    DownloadFinished(String),
    AddFailed { name: String, reason: String },
    ServiceRestarted(String),
    IrcDisconnected(u64),
    /// IRC came back, and how long it had been gone. Only sent when the outage
    /// was reported: an unanswered "IRC is down" is worse than no message, and a
    /// routine reconnect nobody was told about needs no all-clear either.
    IrcReconnected(u64),
    /// The bot finished starting up, with its version and what it is wired to.
    ///
    /// The answer to "did my restart take?", which until now the logs were the
    /// only place to find.
    Started { version: &'static str, integrations: String },
    ConfigRejected(String),
    DiskLow { path: String, percent_free: u64, free_gib: u64 },
    /// Sent by `cmd:testnotify`, and deliberately exempt from every filter and
    /// from the digest delay: its whole purpose is to answer "is this working?"
    /// immediately.
    Test,
}

impl Event {
    /// Whether this event passes a backend's resolved switches.
    ///
    /// `filter` overrides `global` field by field, so a backend states only what
    /// it wants to differ -- ntfy carrying nothing but failures while email
    /// takes everything is two lines of config.
    fn enabled_by(&self, global: &NotificationOptions, filter: &EventFilter) -> bool {
        match self {
            // A test ignores every switch: its whole purpose is to answer "is
            // this working?", including when everything else is off.
            Event::Test => true,
            Event::TorrentAdded(_) => {
                filter.on_torrent_added.unwrap_or(global.on_torrent_added)
            }
            Event::DownloadFinished(_) => {
                filter.on_download_finished.unwrap_or(global.on_download_finished)
            }
            Event::DiskLow { .. } => filter.on_disk_low.unwrap_or(global.on_disk_low),
            // The greeting and the all-clear share a switch: both say "it is
            // working now", and wanting one without the other makes no sense.
            Event::Started { .. } | Event::IrcReconnected(_) => {
                filter.on_start.unwrap_or(global.on_start)
            }
            Event::AddFailed { .. }
            | Event::ServiceRestarted(_)
            | Event::IrcDisconnected(_)
            | Event::ConfigRejected(_) => filter.on_failure.unwrap_or(global.on_failure),
        }
    }

    /// Groups identical repeats. A crash loop restarting rTorrent forty times
    /// should read "rTorrent restarted (x40)", not forty separate lines.
    fn dedup_key(&self) -> String {
        match self {
            Event::TorrentAdded(n) => format!("added:{n}"),
            Event::DownloadFinished(n) => format!("done:{n}"),
            Event::AddFailed { name, .. } => format!("failed:{name}"),
            Event::ServiceRestarted(s) => format!("restart:{s}"),
            Event::IrcDisconnected(_) => "irc".to_string(),
            Event::IrcReconnected(_) => "irc-back".to_string(),
            // Keyed without the duration, and without the integration list, so a
            // crash loop reads "irc2torrent 0.13.0 is up (…) (x40)" rather than
            // forty near-identical lines.
            Event::Started { version, .. } => format!("started:{version}"),
            Event::ConfigRejected(r) => format!("config:{r}"),
            Event::DiskLow { path, .. } => format!("disk:{path}"),
            Event::Test => "test".to_string(),
        }
    }

    fn line(&self) -> String {
        match self {
            Event::TorrentAdded(n) => format!("Added: {n}"),
            Event::DownloadFinished(n) => format!("Finished: {n}"),
            Event::AddFailed { name, reason } => format!("Failed to add {name}: {reason}"),
            Event::ServiceRestarted(s) => format!("{s} was restarted"),
            Event::IrcDisconnected(secs) => {
                format!("IRC has been disconnected for {secs}s")
            }
            Event::IrcReconnected(secs) => format!("IRC is back (was down {secs}s)"),
            Event::Started { version, integrations } => {
                format!("irc2torrent {version} is up ({integrations})")
            }
            Event::ConfigRejected(reason) => format!("Config reload rejected: {reason}"),
            Event::DiskLow { path, percent_free, free_gib } => {
                format!("Low disk space on {path}: {percent_free}% free ({free_gib} GiB)")
            }
            Event::Test => "Test notification -- if you are reading this, it works.".to_string(),
        }
    }

    /// Counted for the daily summary.
    fn summary_bucket(&self) -> Option<&'static str> {
        match self {
            Event::TorrentAdded(_) => Some("added"),
            Event::DownloadFinished(_) => Some("finished"),
            Event::AddFailed { .. } => Some("failed"),
            Event::ServiceRestarted(_) => Some("restarts"),
            _ => None,
        }
    }
}

/// Cheap, cloneable handle for emitting events.
///
/// `Send + Sync` so it works from the config watcher thread and the supervisor's
/// reaper as well as from the bot's own task, none of which share a runtime.
#[derive(Clone)]
pub struct Notifier {
    tx: Option<mpsc::Sender<Event>>,
}

/// The notifier, reachable from code that is not handed one.
///
/// The supervisor runs as PID 1 and starts before the bot builds its object
/// graph, so it cannot be passed a handle at construction. Rather than thread an
/// `Option` through init, `start` publishes here and the supervisor reads it.
/// Anything that happens before the bot is up simply finds the null handle,
/// which is the honest answer -- there was nowhere to send it yet.
static GLOBAL: std::sync::OnceLock<Notifier> = std::sync::OnceLock::new();

/// The process-wide notifier, or a null handle if none is configured yet.
pub fn global() -> Notifier {
    GLOBAL.get().cloned().unwrap_or_else(Notifier::disabled)
}

impl Notifier {
    /// A handle that discards everything, for when notifications are off and in
    /// tests. Keeps call sites free of `if let Some(n) = ...`.
    pub fn disabled() -> Self {
        Self { tx: None }
    }

    /// Whether anything is actually listening.
    ///
    /// Lets callers skip work whose only purpose is to produce events -- the
    /// poller would otherwise make an RPC call every `poll_seconds` for the
    /// large majority of installs that never configure a backend.
    pub fn is_enabled(&self) -> bool {
        self.tx.is_some()
    }

    /// Never blocks and never fails: a full queue drops the event and says so.
    /// A notification is not worth stalling a download for.
    pub fn send(&self, event: Event) {
        let Some(tx) = &self.tx else {
            return;
        };
        if let Err(e) = tx.try_send(event) {
            warn!("Notification dropped ({e}); the notifier is not keeping up.");
        }
    }
}

/// A place to deliver a rendered message.
/// `Sync` as well as `Send`: the dispatcher holds `&dyn Backend` across the
/// await in `deliver_to`, and a shared reference is only `Send` when the
/// referent is `Sync`. Without it the whole task stops being spawnable.
#[async_trait::async_trait]
trait Backend: Send + Sync {
    fn name(&self) -> &'static str;
    async fn deliver(&self, subject: &str, body: &str) -> Result<(), Error>;
    /// Re-attempt anything the backend is holding. Called on each digest tick.
    /// Only IRC needs it -- a message to an absent nick is discarded by the
    /// server, so it is held until the owner reappears.
    async fn retry_pending(&self) {}
}

/// A message a backend could not take yet.
struct Undelivered {
    subject: String,
    body: String,
    attempts: u8,
}

/// The first digest flush comes early, so "the bot is up" does not sit in the
/// buffer for a full digest window. Everything after it uses `digest_seconds`.
const FIRST_FLUSH: Duration = Duration::from_secs(20);

/// How often to re-attempt what could not be delivered.
///
/// Fixed rather than tied to `digest_seconds`: a five-minute digest should not
/// mean a five-minute wait to retry a message that failed because the network
/// was down for twenty seconds -- which is the normal state of a container for
/// the first minute or so of its life.
const RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// Give up after this many, so a permanently wrong SMTP password does not mean
/// a message retried forever.
const MAX_ATTEMPTS: u8 = 6;

/// Messages held per backend. Small on purpose: this exists to cross an outage
/// of a minute or two, not to be a mail spool.
const OUTBOX_DEPTH: usize = 5;

/// A backend plus the event switches that apply to it.
struct Target {
    backend: Box<dyn Backend>,
    filter: EventFilter,
    limit: RateLimit,
    /// What failed to send, oldest first.
    ///
    /// Until now a delivery that failed was logged and dropped, so a
    /// disk-low warning during a thirty-second network blip simply never
    /// arrived. The rate limit is not re-applied on a retry: the message
    /// already spent its allowance when it was first attempted.
    outbox: Vec<Undelivered>,
    /// Which config table this came from, and therefore whether a reload may
    /// rebuild it. Telegram and Slack are `None`: their tables are consumed at
    /// startup by the *command* side too, and rebuilding only half of that would
    /// leave the two roles configured differently.
    section: Option<&'static str>,
}

// ---------------------------------------------------------------------------
// Telegram
// ---------------------------------------------------------------------------

/// Delivers notifications over the same client the command poller uses.
///
/// Notably short, because Telegram needs none of what IRC does: 4096 characters
/// per message and no flood limit worth pacing at this volume, so a digest goes
/// out whole and at once. No presence check, no hold queue, no pacer.
struct TelegramBackend {
    telegram: crate::transports::telegram::Telegram,
}

#[async_trait::async_trait]
impl Backend for TelegramBackend {
    fn name(&self) -> &'static str {
        "telegram"
    }

    async fn deliver(&self, subject: &str, body: &str) -> Result<(), Error> {
        // One message for both: splitting them would be two notifications for
        // one event.
        self.telegram.send_lines(&flat_lines(subject, body)).await
    }
}

// ---------------------------------------------------------------------------
// Slack
// ---------------------------------------------------------------------------

/// Delivers notifications over the same client the Socket Mode listener uses.
///
/// Posting is plain HTTPS (`chat.postMessage`) and needs no socket, so this
/// works even with `commands = false` and nothing listening.
struct SlackBackend {
    slack: crate::transports::slack::Slack,
}

#[async_trait::async_trait]
impl Backend for SlackBackend {
    fn name(&self) -> &'static str {
        "slack"
    }

    async fn deliver(&self, subject: &str, body: &str) -> Result<(), Error> {
        self.slack.send_lines(&flat_lines(subject, body)).await
    }
}

// ---------------------------------------------------------------------------
// Email
// ---------------------------------------------------------------------------

/// SMTP hosts for the providers people actually use, so `address` alone is
/// usually enough. Explicit `host` always wins.
///
/// These hostnames have been stable for many years; the cost of being wrong is
/// one clear log line naming the host that was tried.
const KNOWN_SMTP: &[(&str, &str)] = &[
    ("gmail.com", "smtp.gmail.com"),
    ("googlemail.com", "smtp.gmail.com"),
    ("outlook.com", "smtp-mail.outlook.com"),
    ("hotmail.com", "smtp-mail.outlook.com"),
    ("live.com", "smtp-mail.outlook.com"),
    ("yahoo.com", "smtp.mail.yahoo.com"),
    ("fastmail.com", "smtp.fastmail.com"),
    ("icloud.com", "smtp.mail.me.com"),
    ("me.com", "smtp.mail.me.com"),
    ("gmx.com", "mail.gmx.com"),
    ("gmx.net", "mail.gmx.net"),
    ("yandex.com", "smtp.yandex.com"),
    ("zoho.com", "smtp.zoho.com"),
    ("proton.me", "127.0.0.1"), // Proton Bridge, running locally
    ("protonmail.com", "127.0.0.1"),
];

/// Providers that reject the account password outright and require an
/// app-specific one. Worth naming in the error: "535 authentication failed"
/// sends people hunting in the wrong place.
const NEEDS_APP_PASSWORD: &[&str] = &["gmail.com", "googlemail.com", "yahoo.com", "icloud.com", "me.com"];

pub(crate) fn domain_of(address: &str) -> Option<&str> {
    address.rsplit_once('@').map(|(_, d)| d).filter(|d| !d.is_empty())
}

/// The SMTP host for an address, or None when it cannot be guessed.
pub(crate) fn infer_smtp_host(address: &str) -> Option<&'static str> {
    let domain = domain_of(address)?.to_ascii_lowercase();
    KNOWN_SMTP.iter().find(|(d, _)| *d == domain).map(|(_, h)| *h)
}

pub(crate) fn needs_app_password(address: &str) -> bool {
    domain_of(address)
        .map(|d| d.to_ascii_lowercase())
        .is_some_and(|d| NEEDS_APP_PASSWORD.contains(&d.as_str()))
}

/// Resolve the password from, in order: the config, `password_file`, then
/// `IRC2TORRENT_SMTP_PASSWORD`.
///
/// The file and the variable exist so the secret need not live in a config file
/// that gets bind-mounted into a container and copied around with it.
fn resolve_password(o: &EmailOptions) -> Option<String> {
    if !o.password.is_empty() {
        return Some(o.password.clone());
    }
    if let Some(path) = &o.password_file {
        match std::fs::read_to_string(path) {
            // Trailing newline is near-universal in a secret file and is not
            // part of the password.
            Ok(s) => return Some(s.trim_end_matches(['\n', '\r']).to_string()),
            Err(e) => error!("Could not read smtp password_file {path}: {e}"),
        }
    }
    std::env::var("IRC2TORRENT_SMTP_PASSWORD").ok().filter(|s| !s.is_empty())
}

struct EmailBackend {
    transport: lettre::AsyncSmtpTransport<lettre::Tokio1Executor>,
    from: lettre::message::Mailbox,
    to: lettre::message::Mailbox,
}

impl EmailBackend {
    /// `None` when the section is unusable, having said why. Notifications are a
    /// convenience: a broken one must never stop the bot starting.
    fn build(o: &EmailOptions) -> Option<Self> {
        use lettre::transport::smtp::authentication::Credentials;

        if o.address.trim().is_empty() {
            error!("notifications.email needs an `address`; email is disabled.");
            return None;
        }

        let Some(password) = resolve_password(o) else {
            error!(
                "notifications.email has no password (set `password`, `password_file`, or \
                 IRC2TORRENT_SMTP_PASSWORD); email is disabled."
            );
            return None;
        };

        let host = match o.host.clone().or_else(|| infer_smtp_host(&o.address).map(str::to_string))
        {
            Some(h) => h,
            None => {
                error!(
                    "Cannot infer an SMTP host for {}; set notifications.email.host. \
                     Email is disabled.",
                    o.address
                );
                return None;
            }
        };

        let parse = |what: &str, s: &str| match s.parse::<lettre::message::Mailbox>() {
            Ok(m) => Some(m),
            Err(e) => {
                error!("notifications.email {what} '{s}' is not a valid address: {e}");
                None
            }
        };
        // Both default to the account itself: notifying yourself is the case
        // essentially everyone wants.
        let from = parse("from", o.from.as_deref().unwrap_or(&o.address))?;
        let to = parse("to", o.to.as_deref().unwrap_or(&o.address))?;

        // Port implies the TLS mode, so the user never has to state it: 465 is
        // implicit TLS, everything else is STARTTLS on 587.
        let port = o.port.unwrap_or(587);
        let builder = if port == 465 {
            lettre::AsyncSmtpTransport::<lettre::Tokio1Executor>::relay(&host)
        } else {
            lettre::AsyncSmtpTransport::<lettre::Tokio1Executor>::starttls_relay(&host)
        };

        let transport = match builder {
            Ok(b) => b
                .port(port)
                .timeout(Some(SEND_TIMEOUT))
                .credentials(Credentials::new(o.address.clone(), password))
                .build(),
            Err(e) => {
                error!("Could not set up SMTP to {host}:{port}: {e}");
                return None;
            }
        };

        info!("Email notifications enabled via {host}:{port} as {}.", o.address);
        Some(Self { transport, from, to })
    }
}

#[async_trait::async_trait]
impl Backend for EmailBackend {
    fn name(&self) -> &'static str {
        "email"
    }

    async fn deliver(&self, subject: &str, body: &str) -> Result<(), Error> {
        use lettre::{AsyncTransport, Message};

        let mail = Message::builder()
            .from(self.from.clone())
            .to(self.to.clone())
            .subject(subject)
            .body(body.to_string())?;

        self.transport.send(mail).await.map_err(|e| {
            if e.is_client() && needs_app_password(&self.from.email.to_string()) {
                // The single most common setup failure, and the raw SMTP error
                // gives no hint of the cause.
                Error::msg(format!(
                    "{e} -- this provider rejects account passwords; create an \
                     app password and use that instead"
                ))
            } else {
                Error::msg(e.to_string())
            }
        })?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ntfy
// ---------------------------------------------------------------------------

struct NtfyBackend {
    client: reqwest::Client,
    url: String,
    token: Option<String>,
}

impl NtfyBackend {
    fn build(o: &NtfyOptions) -> Option<Self> {
        let topic = o.topic.trim();
        if topic.is_empty() {
            error!("notifications.ntfy needs a `topic`; ntfy is disabled.");
            return None;
        }
        // A bare topic means the public server; a full URL means self-hosted.
        let url = if topic.starts_with("http://") || topic.starts_with("https://") {
            topic.to_string()
        } else {
            format!("https://ntfy.sh/{topic}")
        };

        info!("ntfy notifications enabled to {url}.");
        Some(Self {
            client: reqwest::Client::builder().timeout(SEND_TIMEOUT).build().ok()?,
            url,
            token: o.token.clone(),
        })
    }
}

#[async_trait::async_trait]
impl Backend for NtfyBackend {
    fn name(&self) -> &'static str {
        "ntfy"
    }

    async fn deliver(&self, subject: &str, body: &str) -> Result<(), Error> {
        // The app name is the title; the events are the message.
        //
        // The title used to be `render`'s subject, which for a single event is
        // that event's line -- so ntfy showed the line as the title and again
        // as the message. Title and message are separate fields here, unlike
        // Telegram or IRC, so the fix is to put the right thing in each rather
        // than to suppress one: ntfy renders the title as the notification
        // heading, which is where "who is telling me this" belongs.
        let title = SUBJECT_PREFIX.trim_end_matches([':', ' ']);
        let mut req = self
            .client
            .post(&self.url)
            // Header, not body: ntfy takes the title out of band so the body
            // stays exactly what the user sees.
            .header("Title", sanitize_header(title))
            .body(body.to_string());
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }

        let resp = req.send().await?;
        if !resp.status().is_success() {
            return Err(Error::msg(format!("ntfy returned HTTP {}", resp.status())));
        }
        Ok(())
    }
}

/// Headers cannot carry control characters, and a torrent name is arbitrary
/// bytes from a `.torrent`. reqwest would reject the request outright; worse
/// would be a header split.
fn sanitize_header(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).take(200).collect()
}

// ---------------------------------------------------------------------------
// IRC private message
// ---------------------------------------------------------------------------

/// Where the current IRC sender lives.
///
/// A `Sender` belongs to one connection, and this bot reconnects with backoff
/// forever, so the backend cannot capture one at startup -- it would go stale at
/// the first disconnect and every later notification would vanish. `IrcProcessor`
/// republishes the sender here on each successful connect.
pub type IrcSenderSlot = std::sync::Arc<std::sync::Mutex<IrcLink>>;

/// The current IRC connection, as far as notifications are concerned.
#[derive(Default)]
pub struct IrcLink {
    /// The paced outbound queue, not the raw sender: notifications and command
    /// replies share one connection, so they must share one flood budget.
    pub sender: Option<crate::irc_processor::irc::Outbound>,
    /// Whether the owner's nick is on the network right now, from a periodic
    /// ISON. A PRIVMSG to an absent nick is discarded by the server with no
    /// error the bot can act on, so without this a notification sent while the
    /// operator is away is simply lost.
    ///
    /// Starts true so an unanswered probe fails *open*: on a network that does
    /// not answer ISON, notifications should still be attempted rather than
    /// silently queued forever.
    pub owner_online: bool,
}

impl IrcLink {
    pub fn connected(sender: crate::irc_processor::irc::Outbound) -> Self {
        Self { sender: Some(sender), owner_online: true }
    }
}

struct IrcBackend {
    link: IrcSenderSlot,
    nick: String,
    /// How long a message may wait for the owner before it is dropped unsent.
    /// From `[notifications.irc] hold_seconds`.
    hold_for: Duration,
    /// Messages held while the owner was away, oldest first, each stamped with
    /// the moment it was held so `hold_for` can be applied on the way out.
    pending: std::sync::Mutex<std::collections::VecDeque<(tokio::time::Instant, String, String)>>,
}

/// How many messages to hold for an absent owner before dropping the oldest.
///
/// A count bound alone was not enough: "away" can mean a fortnight, and twenty
/// messages held for a fortnight still arrive as twenty pieces of stale news the
/// moment the owner reconnects. `hold_for` bounds their age, this bounds how
/// many can accumulate inside that window.
const IRC_HOLD_LIMIT: usize = 20;

#[async_trait::async_trait]
impl Backend for IrcBackend {
    fn name(&self) -> &'static str {
        "irc"
    }

    async fn deliver(&self, subject: &str, body: &str) -> Result<(), Error> {
        // Cloned out of the lock: the guard is not Send and must not be held
        // across anything the compiler might treat as an await point.
        let (sender, online) = {
            let link = self.link.lock().map_err(|_| Error::msg("IRC link lock poisoned"))?;
            (link.sender.clone(), link.owner_online)
        };

        let Some(sender) = sender else {
            self.hold(subject, body);
            return Err(Error::msg("not connected to IRC; message held"));
        };
        if !online {
            // The server discards a PRIVMSG to an absent nick without telling
            // us, so sending now would lose it outright.
            self.hold(subject, body);
            return Err(Error::msg(format!("{} is not online; message held", self.nick)));
        }

        self.flush_held(&sender);
        self.write(&sender, subject, body)
    }

    /// Retry anything held while the owner was away.
    async fn retry_pending(&self) {
        let (sender, online) = match self.link.lock() {
            Ok(link) => (link.sender.clone(), link.owner_online),
            Err(_) => return,
        };
        if let (Some(sender), true) = (sender, online) {
            self.flush_held(&sender);
        }
    }
}

impl IrcBackend {
    /// One PRIVMSG per line: IRC has no multi-line message, and a raw newline
    /// would end the command and hand the remainder to the server.
    fn write(
        &self,
        sender: &crate::irc_processor::irc::Outbound,
        subject: &str,
        body: &str,
    ) -> Result<(), Error> {
        for line in flat_lines(subject, body).iter().filter(|l| !l.trim().is_empty()) {
            let line = crate::irc_processor::irc::sanitize_for_irc(line);
            // Queued, not sent: the pacer on the other end enforces the flood
            // limit that the irc crate advertises but never implements.
            // Uninterruptible: `stop!` cancels command replies, not alerts.
            sender.send_uninterruptible(&self.nick, &line);
        }
        Ok(())
    }

    fn hold(&self, subject: &str, body: &str) {
        // Nothing is held at all when the window is zero, rather than being held
        // and discarded a moment later at the far end.
        if self.hold_for.is_zero() {
            return;
        }
        let Ok(mut queue) = self.pending.lock() else {
            return;
        };
        queue.push_back((tokio::time::Instant::now(), subject.to_string(), body.to_string()));
        // Drop the oldest rather than the newest: recent news is the useful
        // news, and the drop is reported when the queue is flushed.
        while queue.len() > IRC_HOLD_LIMIT {
            queue.pop_front();
        }
    }

    fn flush_held(&self, sender: &crate::irc_processor::irc::Outbound) {
        let held: Vec<(tokio::time::Instant, String, String)> = match self.pending.lock() {
            Ok(mut q) if !q.is_empty() => q.drain(..).collect(),
            _ => return,
        };

        // Age them out here rather than on the way in: what matters is how long
        // a message waited, which is only known once someone is there to
        // receive it. Holding exists to cross a disconnect -- a disk-low warning
        // from last Tuesday is not news, and a queue of them arriving at once is
        // what made this feature something to mute.
        let total = held.len();
        let fresh: Vec<(String, String)> = held
            .into_iter()
            .filter(|(at, _, _)| at.elapsed() < self.hold_for)
            .map(|(_, subject, body)| (subject, body))
            .collect();

        let stale = total - fresh.len();
        if stale > 0 {
            warn!(
                "Dropped {stale} notification(s) that waited more than {}s for {}.",
                self.hold_for.as_secs(),
                self.nick
            );
        }
        if fresh.is_empty() {
            return;
        }

        info!("{} is back; delivering {} held notification(s).", self.nick, fresh.len());
        for (subject, body) in fresh {
            if let Err(e) = self.write(sender, &subject, &body) {
                error!("Could not deliver a held notification: {e}");
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

/// Per-backend token bucket.
struct RateLimit {
    max_per_hour: u32,
    used: u32,
    window_started: tokio::time::Instant,
    suppressed: u32,
}

impl RateLimit {
    fn new(max_per_hour: u32) -> Self {
        Self {
            max_per_hour,
            used: 0,
            window_started: tokio::time::Instant::now(),
            suppressed: 0,
        }
    }

    /// Adopt a new ceiling without resetting the hour: raising `max_per_hour`
    /// should let the next message through, not restart the count.
    fn set_cap(&mut self, max_per_hour: u32) {
        self.max_per_hour = max_per_hour;
    }

    /// Whether a message may go out now, rolling the window if it has expired.
    fn allow(&mut self) -> bool {
        if self.window_started.elapsed() >= Duration::from_secs(3600) {
            self.window_started = tokio::time::Instant::now();
            self.used = 0;
            self.suppressed = 0;
        }
        if self.used < self.max_per_hour {
            self.used += 1;
            true
        } else {
            self.suppressed += 1;
            false
        }
    }

    /// Consumes the suppressed count so it is reported exactly once.
    fn take_suppressed(&mut self) -> u32 {
        std::mem::take(&mut self.suppressed)
    }
}

/// Collapse a buffer of events into one subject and body.
///
/// Public for tests: this is where the "not spammy" promise is actually kept, so
/// it is worth pinning independently of any network.
pub(crate) fn render(events: &[Event]) -> (String, String) {
    // Insertion-ordered so the digest reads chronologically rather than in
    // whatever order a hash map yields.
    let mut order: Vec<String> = Vec::new();
    let mut counts: HashMap<String, (Event, usize)> = HashMap::new();

    for e in events {
        let key = e.dedup_key();
        match counts.get_mut(&key) {
            Some((_, n)) => *n += 1,
            None => {
                order.push(key.clone());
                counts.insert(key, (e.clone(), 1));
            }
        }
    }

    let mut lines = Vec::new();
    for key in &order {
        let (event, n) = &counts[key];
        if *n > 1 {
            lines.push(format!("{} (x{n})", event.line()));
        } else {
            lines.push(event.line());
        }
    }

    let subject = if order.len() == 1 {
        let (event, n) = &counts[&order[0]];
        if *n > 1 {
            format!("{SUBJECT_PREFIX}{} (x{n})", event.line())
        } else {
            format!("{SUBJECT_PREFIX}{}", event.line())
        }
    } else {
        format!("{SUBJECT_PREFIX}{} events", events.len())
    };

    (subject, lines.join("\n"))
}

/// What every subject is prefixed with, so a notification says who sent it.
pub(crate) const SUBJECT_PREFIX: &str = "irc2torrent: ";

/// The single-message form, for transports with no separate subject field.
///
/// `render` returns a subject that, for a *single* event, is that event's line
/// with the prefix on it -- which is right for email, where the subject is a
/// real header and the body still has to carry the detail. Telegram, Slack and
/// IRC have no such field: they concatenated subject and body, and so printed
/// the line twice for every single-event notification, which is the common case.
///
/// Dropping the body line the subject already carries is exact rather than
/// heuristic: the subject is only ever the prefix plus that line, `(xN)` suffix
/// included, so stripping the prefix and comparing cannot match a line that
/// merely looks similar. With several events the subject is "N events" and
/// nothing is suppressed.
fn flat_lines(subject: &str, body: &str) -> Vec<String> {
    let carried = subject.strip_prefix(SUBJECT_PREFIX);
    std::iter::once(subject.to_string())
        .chain(
            body.lines()
                .filter(|l| carried != Some(*l))
                .map(str::to_string),
        )
        .collect()
}

/// Long-running task that owns the targets and does the shaping.
struct Dispatcher {
    rx: mpsc::Receiver<Event>,
    targets: Vec<Target>,
    /// The last config adopted. Compared against a fresh read to decide what,
    /// if anything, has to be rebuilt.
    options: NotificationOptions,
    source: crate::config::config::SharedNotificationOptions,
    /// Kept so the IRC backend can be rebuilt on a reload; it needs the sender
    /// slot and the owner's nick, neither of which is in `[notifications]`.
    irc_owner: Option<(IrcSenderSlot, String)>,
    /// Every event that at least one target wants. Which of them each target
    /// actually receives is decided at flush time, since the filters differ.
    buffer: Vec<Event>,
    /// Running tally for the daily summary.
    totals: HashMap<&'static str, usize>,
}

impl Dispatcher {
    async fn run(mut self) {
        let mut digest = self.options.digest_seconds.max(1);
        let period = Duration::from_secs(digest);

        // The first flush comes early and every one after it on the configured
        // window. The startup greeting is the reason: with the default 300s
        // digest it otherwise sat in the buffer for five minutes, which is long
        // enough to look broken -- and looking broken is the one thing a message
        // saying "I am up" must not do.
        //
        // `interval_at` rather than `interval`, so there is no immediate tick to
        // skip and the early deadline is the first one.
        let mut flush =
            tokio::time::interval_at(tokio::time::Instant::now() + FIRST_FLUSH.min(period), period);

        // Fixed cadence, independent of the digest: what this retries failed
        // because something was briefly unreachable, and twenty seconds later is
        // when to find out, not five minutes later.
        let mut retry = tokio::time::interval(RETRY_INTERVAL);
        // Delay, not the default Burst: a backend can take up to SEND_TIMEOUT,
        // so a slow retry round could overrun its own period -- and catching up
        // would fire the missed ticks back to back, hammering a backend that is
        // already struggling.
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        retry.tick().await;

        let mut daily = tokio::time::interval(Duration::from_secs(86_400));
        daily.tick().await;

        loop {
            // Adopt any config change before deciding anything with it. Once per
            // wake-up, not per message: events arrive at human rates, and this
            // is a small clone plus a comparison.
            self.refresh();
            if self.options.digest_seconds.max(1) != digest {
                digest = self.options.digest_seconds.max(1);
                info!("Notification digest window is now {digest}s.");
                flush = tokio::time::interval(Duration::from_secs(digest));
                flush.tick().await;
            }

            tokio::select! {
                received = self.rx.recv() => {
                    let Some(event) = received else {
                        // Every sender is gone: the process is shutting down.
                        self.flush().await;
                        return;
                    };
                    if let Some(bucket) = event.summary_bucket() {
                        *self.totals.entry(bucket).or_default() += 1;
                    }

                    // Keep it only if some target would take it, so a disabled
                    // event costs nothing downstream.
                    let wanted = self
                        .targets
                        .iter()
                        .any(|t| event.enabled_by(&self.options, &t.filter));
                    if !wanted {
                        continue;
                    }

                    // A test must not sit out the digest window: someone is
                    // watching for it right now.
                    if event == Event::Test {
                        self.deliver(&[Event::Test]).await;
                    } else {
                        self.buffer.push(event);
                    }
                }
                _ = flush.tick() => self.flush().await,
                _ = retry.tick() => {
                    // Give a backend a chance to clear what it held: IRC parks
                    // messages while the owner is offline, and this tick is what
                    // notices they came back.
                    for target in &self.targets {
                        target.backend.retry_pending().await;
                    }
                    self.retry_outboxes().await;
                }
                _ = daily.tick(), if self.wants_summary() => self.send_summary().await,
            }
        }
    }

    /// Re-read `[notifications]` and adopt whatever changed.
    ///
    /// The scalars -- the event switches, the digest window, the rate cap -- are
    /// simply the new values. The backends are objects with state (an SMTP
    /// connection, IRC's queue of messages held while the owner is offline), so
    /// only the ones whose own table changed are torn down and rebuilt; editing
    /// `digest_seconds` must not cost you a held IRC message.
    fn refresh(&mut self) {
        let Some(fresh) = self.source.get() else { return };
        if fresh == self.options {
            return;
        }

        let changed: Vec<&'static str> = RELOADABLE
            .iter()
            .copied()
            .filter(|section| match *section {
                "email" => fresh.email != self.options.email,
                "ntfy" => fresh.ntfy != self.options.ntfy,
                "irc" => fresh.irc != self.options.irc,
                _ => false,
            })
            .collect();

        if !changed.is_empty() {
            self.targets.retain(|t| !t.section.is_some_and(|s| changed.contains(&s)));
            self.targets.extend(build_section_targets(&fresh, &self.irc_owner, &changed));
            info!(
                "Notification backends reloaded ({}); {} active.",
                changed.join(", "),
                self.targets.len()
            );
        }

        if fresh.max_per_hour != self.options.max_per_hour {
            for target in &mut self.targets {
                target.limit.set_cap(fresh.max_per_hour);
            }
        }

        self.options = fresh;
    }

    /// Whether any target takes the daily summary; without this the timer would
    /// fire and render a report nobody receives.
    fn wants_summary(&self) -> bool {
        self.targets
            .iter()
            .any(|t| t.filter.daily_summary.unwrap_or(self.options.daily_summary))
    }

    async fn flush(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let events = std::mem::take(&mut self.buffer);
        self.deliver(&events).await;
    }

    async fn send_summary(&mut self) {
        let totals = std::mem::take(&mut self.totals);
        let body = ["added", "finished", "failed", "restarts"]
            .iter()
            .map(|k| format!("{k}: {}", totals.get(k).copied().unwrap_or(0)))
            .collect::<Vec<_>>()
            .join("\n");

        for i in 0..self.targets.len() {
            if !self.targets[i].filter.daily_summary.unwrap_or(self.options.daily_summary) {
                continue;
            }
            self.deliver_to(i, "irc2torrent: daily summary", &body).await;
        }
    }

    /// Render and send, per target, since each may have filtered out a different
    /// subset -- and therefore gets a different subject and body.
    async fn deliver(&mut self, events: &[Event]) {
        for i in 0..self.targets.len() {
            let mine: Vec<Event> = events
                .iter()
                .filter(|e| e.enabled_by(&self.options, &self.targets[i].filter))
                .cloned()
                .collect();
            if mine.is_empty() {
                continue;
            }
            let (subject, body) = render(&mine);
            self.deliver_to(i, &subject, &body).await;
        }
    }

    async fn deliver_to(&mut self, index: usize, subject: &str, body: &str) {
        if !self.targets[index].limit.allow() {
            return;
        }
        // Report what the cap ate, so a quiet hour is never mistaken for a calm
        // one.
        let suppressed = self.targets[index].limit.take_suppressed();
        let body = if suppressed > 0 {
            format!(
                "{body}\n\n({suppressed} further notifications were suppressed by max_per_hour.)"
            )
        } else {
            body.to_string()
        };

        let target = &self.targets[index];
        if let Err(e) = target.backend.deliver(subject, &body).await {
            let name = target.backend.name();
            error!("Could not send {name} notification: {e}; will retry.");

            let outbox = &mut self.targets[index].outbox;
            if outbox.len() >= OUTBOX_DEPTH {
                // Oldest first: during a long outage the recent events are the
                // ones still worth reading.
                outbox.remove(0);
                warn!("{name}'s backlog is full; dropped the oldest held notification.");
            }
            outbox.push(Undelivered {
                subject: subject.to_string(),
                body,
                attempts: 1,
            });
        }
    }

    /// Re-attempt everything held, oldest first.
    ///
    /// The rate limit is deliberately not consulted: the message already spent
    /// its allowance on the attempt that failed, and charging it again would let
    /// a network blip eat the hour's budget.
    async fn retry_outboxes(&mut self) {
        for index in 0..self.targets.len() {
            if self.targets[index].outbox.is_empty() {
                continue;
            }

            // Taken out for the duration: the backend is borrowed across the
            // await, so the outbox cannot be mutated in place while sending.
            let held = std::mem::take(&mut self.targets[index].outbox);
            let name = self.targets[index].backend.name();
            let mut still_held = Vec::new();

            for mut message in held {
                match self.targets[index].backend.deliver(&message.subject, &message.body).await {
                    Ok(()) => info!(
                        "Delivered a held {name} notification on attempt {}.",
                        message.attempts + 1
                    ),
                    Err(e) => {
                        message.attempts += 1;
                        if message.attempts >= MAX_ATTEMPTS {
                            error!(
                                "Giving up on a {name} notification after {} attempts: {e}",
                                message.attempts
                            );
                        } else {
                            still_held.push(message);
                        }
                    }
                }
            }

            self.targets[index].outbox = still_held;
        }
    }
}

/// Build the backends belonging to `[notifications]`, the ones a reload may
/// rebuild.
///
/// `quiet` suppresses the "enabled" lines, so a reload does not repeat what was
/// already said at startup for a backend that has not changed.
fn build_section_targets(
    options: &NotificationOptions,
    irc_owner: &Option<(IrcSenderSlot, String)>,
    want: &[&'static str],
) -> Vec<Target> {
    let cap = options.max_per_hour;
    let mut targets = Vec::new();

    if want.contains(&"email") {
        if let Some(email) = &options.email {
            if let Some(b) = EmailBackend::build(email) {
                targets.push(Target {
                    backend: Box::new(b),
                    filter: email.events.clone(),
                    limit: RateLimit::new(cap),
                    outbox: Vec::new(),
                    section: Some("email"),
                });
            }
        }
    }
    if want.contains(&"ntfy") {
        if let Some(ntfy) = &options.ntfy {
            if let Some(b) = NtfyBackend::build(ntfy) {
                targets.push(Target {
                    backend: Box::new(b),
                    filter: ntfy.events.clone(),
                    limit: RateLimit::new(cap),
                    outbox: Vec::new(),
                    section: Some("ntfy"),
                });
            }
        }
    }
    if want.contains(&"irc") {
        if let Some(irc_opts) = &options.irc {
            match irc_owner {
                Some((link, nick)) => {
                    info!("IRC notifications enabled to {nick}.");
                    targets.push(Target {
                        backend: Box::new(IrcBackend {
                            link: link.clone(),
                            nick: nick.clone(),
                            hold_for: Duration::from_secs(irc_opts.hold_seconds),
                            pending: Default::default(),
                        }),
                        filter: irc_opts.events.clone(),
                        limit: RateLimit::new(cap),
                        outbox: Vec::new(),
                        section: Some("irc"),
                    });
                }
                None => error!(
                    "[notifications.irc] is present, but there is no owner nick to message: set \
                     command_options.security_mode to IrcUserName. IRC notifications are disabled."
                ),
            }
        }
    }

    targets
}

/// Every backend a reload is allowed to rebuild.
const RELOADABLE: &[&str] = &["email", "ntfy", "irc"];

/// Build the backends and start the dispatcher.
pub fn start(
    options: NotificationOptions,
    // Re-read on every wake-up, which is what makes [notifications] live. See
    // `SharedNotificationOptions`.
    source: crate::config::config::SharedNotificationOptions,
    irc_owner: Option<(IrcSenderSlot, String)>,
    // Built by the caller and shared with the command poller: one client, one
    // token, so the two roles cannot end up configured differently.
    telegram: Option<(crate::transports::telegram::Telegram, EventFilter)>,
    slack: Option<(crate::transports::slack::Slack, EventFilter)>,
) -> Notifier {
    let cap = options.max_per_hour;
    let mut targets = build_section_targets(&options, &irc_owner, RELOADABLE);

    // Pinned: `section: None`, so a reload leaves them alone.
    if let Some((client, events)) = telegram {
        info!("Telegram notifications enabled.");
        targets.push(Target {
            backend: Box::new(TelegramBackend { telegram: client }),
            filter: events,
            limit: RateLimit::new(cap),
            outbox: Vec::new(),
            section: None,
        });
    }
    if let Some((client, events)) = slack {
        info!("Slack notifications enabled.");
        targets.push(Target {
            backend: Box::new(SlackBackend { slack: client }),
            filter: events,
            limit: RateLimit::new(cap),
            outbox: Vec::new(),
            section: None,
        });
    }

    if targets.is_empty() {
        // Still started, unlike before: adding a backend to options.toml and
        // saving now works without a restart, and it cannot do that if there is
        // no dispatcher listening. An idle one is a parked task and a timer.
        info!("No notification backends configured (add one to options.toml; no restart needed).");
    }

    let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
    let dispatcher = Dispatcher {
        rx,
        targets,
        options,
        source,
        irc_owner,
        buffer: Vec::new(),
        totals: HashMap::new(),
    };
    // tokio::spawn, not spawn_local: there is no LocalSet -- main awaits a
    // single future -- and everything the dispatcher holds is Send precisely so
    // it can live on its own task, away from the bot's Rc-based world.
    tokio::spawn(dispatcher.run());

    let notifier = Notifier { tx: Some(tx) };
    // Ignore a second call: only the first set wins, and start() runs once.
    let _ = GLOBAL.set(notifier.clone());
    notifier
}

// ---------------------------------------------------------------------------
// Pollers
// ---------------------------------------------------------------------------

/// Free space on `path`, as (percent free, free GiB).
///
/// `statvfs` through libc, which is already a dependency -- no crate needed for
/// one syscall. Uses `f_bavail` (blocks available to an unprivileged process)
/// rather than `f_bfree`, since the root-reserved margin is not space anyone
/// here can use.
pub(crate) fn free_space(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: zeroed statvfs is a valid starting value, and the path is a
    // NUL-terminated C string that outlives the call.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }

    let block = stat.f_frsize as u64;
    let total = stat.f_blocks as u64;
    let avail = stat.f_bavail as u64;
    if total == 0 {
        return None;
    }

    Some((avail * 100 / total, avail.saturating_mul(block) / (1024 * 1024 * 1024)))
}

/// Watch for torrents reaching 100% and for the disk filling up.
///
/// One task for both because they share a cadence and neither is worth its own.
/// It only ever reads, so a failure is logged and retried on the next tick.
pub async fn poll(
    tp: std::rc::Rc<crate::torrent_processor::torrent::TorrentProcessor>,
    mut options: NotificationOptions,
    source: crate::config::config::SharedNotificationOptions,
    notifier: Notifier,
) {
    use std::collections::HashSet;

    // No notifier at all means nothing was ever wired up -- only the case in
    // tests, since `start` always returns an enabled one now.
    //
    // Parks forever rather than returning. This runs as a `select!` arm beside
    // the IRC listener, and a branch that completes cancels the other one -- so
    // returning here would quietly shut the bot down.
    if !notifier.is_enabled() {
        std::future::pending::<()>().await;
        return;
    }

    let mut finished: HashSet<String> = HashSet::new();
    let mut seeded = false;
    // Warn once per crossing, not once per tick, or a full disk means a message
    // every poll_seconds until someone frees space.
    let mut disk_warned = false;

    let mut period = options.poll_seconds.max(10);
    let mut tick = tokio::time::interval(Duration::from_secs(period));

    loop {
        tick.tick().await;

        // Live, like the dispatcher's: turning on_download_finished on should
        // not need a restart, and neither should retuning the interval.
        if let Some(fresh) = source.get() {
            options = fresh;
        }
        if options.poll_seconds.max(10) != period {
            period = options.poll_seconds.max(10);
            tick = tokio::time::interval(Duration::from_secs(period));
            tick.tick().await;
        }

        // Nothing worth polling for: skip the work, but keep ticking so turning
        // it on later is picked up. This is a timer wake-up, not an RPC call.
        if !(options.on_download_finished || options.on_disk_low) {
            continue;
        }

        if options.on_download_finished {
            if let Some(rows) = tp.get_completed().await {
                let current: HashSet<String> = rows.iter().map(|(h, _)| h.clone()).collect();

                if !seeded {
                    // Everything already complete at startup is history, not
                    // news. Without this the first tick after every restart
                    // announces the entire finished library.
                    finished = current;
                    seeded = true;
                } else {
                    for (hash, name) in rows {
                        if finished.insert(hash) {
                            notifier.send(Event::DownloadFinished(name));
                        }
                    }
                    // Forget torrents that were removed, so re-adding one can
                    // report finishing again and the set cannot grow forever.
                    finished.retain(|h| current.contains(h));
                }
            }
        }

        if options.on_disk_low {
            let path = std::path::Path::new(&options.disk_path);
            match free_space(path) {
                Some((percent, gib)) if (percent as f64) < options.disk_warn_percent => {
                    if !disk_warned {
                        disk_warned = true;
                        notifier.send(Event::DiskLow {
                            path: options.disk_path.clone(),
                            percent_free: percent,
                            free_gib: gib,
                        });
                    }
                }
                Some(_) => disk_warned = false,
                None => {
                    // A missing or unreadable path is a config problem, not a
                    // full disk; say so once rather than every tick.
                    if !disk_warned {
                        disk_warned = true;
                        warn!(
                            "Cannot check free space on {} -- set notifications.disk_path.",
                            options.disk_path
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_known_provider_needs_only_an_address() {
        assert_eq!(infer_smtp_host("me@gmail.com"), Some("smtp.gmail.com"));
        assert_eq!(infer_smtp_host("Me@GMail.COM"), Some("smtp.gmail.com"));
        assert_eq!(infer_smtp_host("me@fastmail.com"), Some("smtp.fastmail.com"));
    }

    /// An unknown domain must fall through to an explicit host rather than
    /// guessing something like smtp.<domain>, which fails at connect time with
    /// nothing pointing at the cause.
    #[test]
    fn an_unknown_provider_is_not_guessed_at() {
        assert_eq!(infer_smtp_host("me@my-own-domain.example"), None);
        assert_eq!(infer_smtp_host("not-an-address"), None);
        assert_eq!(infer_smtp_host("me@"), None);
    }

    #[test]
    fn providers_that_reject_account_passwords_are_flagged() {
        assert!(needs_app_password("me@gmail.com"));
        assert!(needs_app_password("me@icloud.com"));
        assert!(!needs_app_password("me@fastmail.com"));
    }

    /// The anti-spam promise: forty restarts are one line with a count.
    #[test]
    fn repeats_collapse_into_a_count() {
        let events: Vec<Event> =
            (0..40).map(|_| Event::ServiceRestarted("rtorrent".into())).collect();
        let (subject, body) = render(&events);

        assert!(subject.contains("(x40)"), "{subject}");
        assert_eq!(body.lines().count(), 1, "{body}");
    }

    #[test]
    fn distinct_events_each_get_a_line_in_order() {
        let events = vec![
            Event::TorrentAdded("Alpha".into()),
            Event::DownloadFinished("Beta".into()),
            Event::TorrentAdded("Alpha".into()),
        ];
        let (subject, body) = render(&events);

        // Three events, two distinct lines, and the repeat counted.
        assert!(subject.contains("3 events"), "{subject}");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "{body}");
        assert!(lines[0].starts_with("Added: Alpha"), "{body}");
        assert!(lines[0].contains("(x2)"), "{body}");
        assert!(lines[1].starts_with("Finished: Beta"), "{body}");
    }

    #[test]
    fn a_single_event_names_itself_in_the_subject() {
        let (subject, body) = render(&[Event::DownloadFinished("Some Release".into())]);
        assert_eq!(subject, "irc2torrent: Finished: Some Release");
        assert_eq!(body, "Finished: Some Release");
    }

    /// A transport with no subject field must not print the event twice.
    ///
    /// The subject for a single event *is* that event's line, which is correct
    /// for email but meant Telegram, Slack and IRC sent
    ///   irc2torrent: Finished: X
    ///   Finished: X
    /// for every single-event notification -- the common case.
    #[test]
    fn a_flat_transport_states_a_single_event_once() {
        let (subject, body) = render(&[Event::DownloadFinished("Some Release".into())]);
        assert_eq!(flat_lines(&subject, &body), vec!["irc2torrent: Finished: Some Release"]);
    }

    /// The same when the one event repeated: the subject carries the `(xN)`, so
    /// the body line matches it exactly and is still suppressed.
    #[test]
    fn a_flat_transport_states_a_repeated_event_once() {
        let events: Vec<Event> = (0..3).map(|_| started()).collect();
        let (subject, body) = render(&events);
        let lines = flat_lines(&subject, &body);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("(x3)"), "{lines:?}");
    }

    /// With more than one distinct event the subject is a count, so every line
    /// still has to be sent -- suppression must not eat real content.
    #[test]
    fn a_flat_transport_keeps_every_line_of_a_digest() {
        let events =
            vec![Event::TorrentAdded("Alpha".into()), Event::DownloadFinished("Beta".into())];
        let (subject, body) = render(&events);
        let lines = flat_lines(&subject, &body);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0], "irc2torrent: 2 events");
        assert!(lines[1].contains("Alpha"), "{lines:?}");
        assert!(lines[2].contains("Beta"), "{lines:?}");
    }

    /// A line that merely resembles the subject is not suppressed: the match is
    /// against the whole line after the prefix, not a substring.
    #[test]
    fn suppression_matches_the_whole_line_not_a_substring() {
        let lines = flat_lines("irc2torrent: Finished: Some Release", "Finished: Some");
        assert_eq!(lines.len(), 2, "{lines:?}");
    }

    fn failure() -> Event {
        Event::AddFailed { name: "x".into(), reason: "y".into() }
    }

    fn started() -> Event {
        Event::Started { version: "9.9.9", integrations: "ntfy, telegram".into() }
    }

    /// The greeting has to name what it came up with: the reason for reading it
    /// is usually that you just changed the config, and this is what confirms
    /// the change was picked up.
    #[test]
    fn the_greeting_names_the_version_and_the_integrations() {
        let line = started().line();
        assert!(line.contains("9.9.9"), "{line}");
        assert!(line.contains("ntfy, telegram"), "{line}");
    }

    /// A crash-looping container restarts every few seconds. Forty greetings
    /// must collapse the way forty restarts already do, or the anti-spam design
    /// has a hole in exactly the situation it exists for.
    #[test]
    fn repeated_greetings_collapse_into_a_count() {
        let events: Vec<Event> = (0..40).map(|_| started()).collect();
        let (subject, body) = render(&events);

        assert!(subject.contains("(x40)"), "{subject}");
        assert_eq!(body.lines().count(), 1, "{body}");
    }

    /// The greeting and the all-clear are one switch, separate from failures:
    /// somebody who wants to be told only when things break should not be told
    /// every time the bot starts.
    #[test]
    fn the_greeting_follows_on_start_and_not_on_failure() {
        let only_failures =
            NotificationOptions { on_start: false, on_failure: true, ..Default::default() };
        assert!(!started().enabled_by(&only_failures, &EventFilter::default()));
        assert!(!Event::IrcReconnected(90).enabled_by(&only_failures, &EventFilter::default()));
        // The failure itself still gets through.
        assert!(Event::IrcDisconnected(90).enabled_by(&only_failures, &EventFilter::default()));

        // And a backend can want the greeting while the global says no.
        let wants_it = EventFilter { on_start: Some(true), ..EventFilter::default() };
        assert!(started().enabled_by(&only_failures, &wants_it));
    }

    /// On by default: it is the answer to "did my restart work?".
    #[test]
    fn the_greeting_is_on_out_of_the_box() {
        assert!(started().enabled_by(&NotificationOptions::default(), &EventFilter::default()));
    }

    // -----------------------------------------------------------------------
    // Holding and retrying what could not be delivered
    // -----------------------------------------------------------------------

    /// Refuses the first `failures` deliveries, the way an unreachable network
    /// does while a container is still being wired up.
    #[derive(Default)]
    struct Flaky {
        failures_left: std::sync::Mutex<u8>,
        delivered: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Backend for std::sync::Arc<Flaky> {
        fn name(&self) -> &'static str {
            "flaky"
        }

        async fn deliver(&self, subject: &str, _body: &str) -> Result<(), Error> {
            let mut left = self.failures_left.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                return Err(Error::msg("unreachable"));
            }
            self.delivered.lock().unwrap().push(subject.to_string());
            Ok(())
        }
    }

    fn flaky_dispatcher(failures: u8) -> (Dispatcher, std::sync::Arc<Flaky>) {
        let backend = std::sync::Arc::new(Flaky {
            failures_left: std::sync::Mutex::new(failures),
            delivered: Default::default(),
        });
        let (_tx, rx) = mpsc::channel(4);

        let dispatcher = Dispatcher {
            rx,
            targets: vec![Target {
                backend: Box::new(backend.clone()),
                filter: EventFilter::default(),
                limit: RateLimit::new(20),
                outbox: Vec::new(),
                section: Some("ntfy"),
            }],
            options: NotificationOptions::default(),
            source: Config::default_for_test().shared_notifications(),
            irc_owner: None,
            buffer: Vec::new(),
            totals: HashMap::new(),
        };
        (dispatcher, backend)
    }

    /// The case from the field: a container starts its bot before the network is
    /// up, so the greeting cannot be sent for the first minute or so. Until now
    /// a failed delivery was logged and dropped, which meant it never arrived at
    /// all -- and the same was true of a disk-low warning during any blip.
    #[tokio::test]
    async fn a_delivery_that_fails_is_held_and_retried_until_it_lands() {
        let (mut d, backend) = flaky_dispatcher(2);

        d.deliver_to(0, "irc2torrent is up", "body").await;
        assert_eq!(d.targets[0].outbox.len(), 1, "the failure must be held, not dropped");
        assert!(backend.delivered.lock().unwrap().is_empty());

        d.retry_outboxes().await;
        assert_eq!(d.targets[0].outbox.len(), 1, "second attempt fails too");
        assert_eq!(d.targets[0].outbox[0].attempts, 2);

        d.retry_outboxes().await;
        assert!(d.targets[0].outbox.is_empty(), "delivered, so no longer held");
        assert_eq!(*backend.delivered.lock().unwrap(), vec!["irc2torrent is up".to_string()]);
    }

    /// A retry must not be charged to the hourly ceiling twice: the message
    /// already spent its allowance on the attempt that failed, and a network
    /// blip must not be able to eat the whole hour's budget.
    #[tokio::test]
    async fn retrying_does_not_spend_the_rate_limit_again() {
        let (mut d, _backend) = flaky_dispatcher(1);

        d.deliver_to(0, "subject", "body").await;
        assert_eq!(d.targets[0].limit.used, 1);

        d.retry_outboxes().await;
        assert!(d.targets[0].outbox.is_empty());
        assert_eq!(d.targets[0].limit.used, 1, "the retry is the same message");
    }

    /// A wrong SMTP password fails forever. Holding that message for the life of
    /// the process would be a leak dressed up as resilience.
    #[tokio::test]
    async fn a_message_that_never_lands_is_eventually_given_up_on() {
        let (mut d, _backend) = flaky_dispatcher(u8::MAX);

        d.deliver_to(0, "subject", "body").await;
        for _ in 0..MAX_ATTEMPTS {
            d.retry_outboxes().await;
        }

        assert!(d.targets[0].outbox.is_empty(), "should have given up by now");
    }

    /// During a long outage the recent events are the ones worth reading, and
    /// the backlog is a bridge over a blip rather than a mail spool.
    #[tokio::test]
    async fn the_backlog_is_bounded_and_drops_the_oldest() {
        let (mut d, _backend) = flaky_dispatcher(u8::MAX);

        for i in 0..OUTBOX_DEPTH + 3 {
            d.deliver_to(0, &format!("subject {i}"), "body").await;
        }

        assert_eq!(d.targets[0].outbox.len(), OUTBOX_DEPTH);
        assert_eq!(
            d.targets[0].outbox[0].subject, "subject 3",
            "the three oldest should have been dropped"
        );
    }

    // -----------------------------------------------------------------------
    // Holding for an absent owner
    //
    // A PRIVMSG to a nick that is not on the network is discarded by the server
    // without an error, so messages wait for the owner to come back. They used
    // to wait indefinitely: an owner returning after a weekend was handed a
    // weekend of alerts at once, all of them describing situations that had
    // resolved long before. Bounding the count was not enough -- twenty stale
    // messages are still twenty stale messages.
    // -----------------------------------------------------------------------

    fn offline_irc_backend(hold_for: Duration) -> IrcBackend {
        IrcBackend {
            // No sender: every delivery is held, which is the case under test.
            link: IrcSenderSlot::default(),
            nick: "owner".into(),
            hold_for,
            pending: Default::default(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_notification_is_dropped_once_it_is_too_old_to_be_news() {
        let backend = offline_irc_backend(Duration::from_secs(900));

        assert!(backend.deliver("disk is low", "body").await.is_err(), "nobody to send to");
        assert_eq!(backend.pending.lock().unwrap().len(), 1, "so it is held");

        tokio::time::advance(Duration::from_secs(901)).await;

        let (outbound, mut rx) = crate::irc_processor::irc::Outbound::for_test();
        backend.flush_held(&outbound);

        assert!(backend.pending.lock().unwrap().is_empty(), "the stale message is gone");
        assert!(rx.try_recv().is_err(), "and was not delivered");
    }

    /// The point of holding in the first place: a short disconnect must still be
    /// crossed, or the feature does nothing at all.
    #[tokio::test(start_paused = true)]
    async fn a_held_notification_still_arrives_after_a_short_absence() {
        let backend = offline_irc_backend(Duration::from_secs(900));

        assert!(backend.deliver("disk is low", "body").await.is_err());
        tokio::time::advance(Duration::from_secs(60)).await;

        let (outbound, mut rx) = crate::irc_processor::irc::Outbound::for_test();
        backend.flush_held(&outbound);

        assert!(backend.pending.lock().unwrap().is_empty());
        let sent = rx.try_recv().expect("a minute old is still news");
        assert_eq!(sent.text(), "disk is low");
    }

    /// Fresh and stale in the same queue: only the stale ones go.
    #[tokio::test(start_paused = true)]
    async fn only_the_stale_part_of_the_backlog_is_dropped() {
        let backend = offline_irc_backend(Duration::from_secs(900));

        for i in 0..2 {
            assert!(backend.deliver(&format!("old {i}"), "body").await.is_err());
        }
        tokio::time::advance(Duration::from_secs(901)).await;
        for i in 0..2 {
            assert!(backend.deliver(&format!("new {i}"), "body").await.is_err());
        }

        let (outbound, mut rx) = crate::irc_processor::irc::Outbound::for_test();
        backend.flush_held(&outbound);

        let delivered: Vec<String> =
            std::iter::from_fn(|| rx.try_recv().ok()).map(|m| m.text().to_string()).collect();
        // One PRIVMSG per line, so each surviving notification appears as its
        // subject followed by its body. The stale pair contributes neither.
        assert_eq!(delivered, vec!["new 0", "body", "new 1", "body"]);
    }

    /// `hold_seconds = 0` is "do not hold": nothing is queued, rather than
    /// queued and thrown away at the far end.
    #[tokio::test(start_paused = true)]
    async fn a_zero_window_holds_nothing_at_all() {
        let backend = offline_irc_backend(Duration::ZERO);

        assert!(backend.deliver("disk is low", "body").await.is_err());
        assert!(backend.pending.lock().unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // Live reload
    // -----------------------------------------------------------------------

    use crate::config::config::{Config, IrcNotifyOptions, NtfyOptions};

    fn with_ntfy_and_irc(topic: &str) -> NotificationOptions {
        NotificationOptions {
            ntfy: Some(NtfyOptions {
                topic: topic.into(),
                token: None,
                events: EventFilter::default(),
            }),
            irc: Some(IrcNotifyOptions::default()),
            ..NotificationOptions::default()
        }
    }

    /// A dispatcher wired to a real `Config`, so `refresh` reads through the
    /// same handle the watcher thread writes through.
    fn dispatcher_for(cfg: &Config, options: NotificationOptions) -> Dispatcher {
        let irc_owner = Some((IrcSenderSlot::default(), "owner".to_string()));
        let targets = build_section_targets(&options, &irc_owner, RELOADABLE);
        let (_tx, rx) = mpsc::channel(4);

        Dispatcher {
            rx,
            targets,
            options,
            source: cfg.shared_notifications(),
            irc_owner,
            buffer: Vec::new(),
            totals: HashMap::new(),
        }
    }

    fn names(d: &Dispatcher) -> Vec<&'static str> {
        d.targets.iter().map(|t| t.backend.name()).collect()
    }

    /// Editing one backend must not tear down the others. IRC holds messages
    /// while the owner is offline and email holds an SMTP connection; losing
    /// either because an unrelated table was touched is a worse bug than the
    /// restart this replaces.
    #[test]
    fn only_the_backend_whose_table_changed_is_rebuilt() {
        let mut cfg = Config::default_for_test();
        cfg.set_notifications_for_test(with_ntfy_and_irc("first-topic"));
        let mut d = dispatcher_for(&cfg, with_ntfy_and_irc("first-topic"));
        assert_eq!(names(&d), vec!["ntfy", "irc"]);

        // Spend some of each target's hourly allowance: surviving state is how
        // "was not rebuilt" is observable from here.
        for t in &mut d.targets {
            assert!(t.limit.allow());
        }

        cfg.set_notifications_for_test(with_ntfy_and_irc("second-topic"));
        d.refresh();

        let ntfy = d.targets.iter().find(|t| t.backend.name() == "ntfy").expect("ntfy");
        let irc = d.targets.iter().find(|t| t.backend.name() == "irc").expect("irc");
        assert_eq!(ntfy.limit.used, 0, "ntfy changed, so it was rebuilt");
        assert_eq!(irc.limit.used, 1, "irc did not change and must have been left alone");
    }

    /// Adding a backend to a config that had none is the case that used to be
    /// impossible without a restart -- and the one where the dispatcher would
    /// not even have been running.
    #[test]
    fn a_backend_added_after_startup_is_picked_up() {
        let mut cfg = Config::default_for_test();
        let mut d = dispatcher_for(&cfg, NotificationOptions::default());
        assert!(d.targets.is_empty());

        cfg.set_notifications_for_test(with_ntfy_and_irc("new-topic"));
        d.refresh();

        assert_eq!(names(&d), vec!["ntfy", "irc"]);
    }

    /// Removing one takes effect too, or the config would be a one-way door.
    #[test]
    fn a_backend_removed_after_startup_stops_being_used() {
        let mut cfg = Config::default_for_test();
        cfg.set_notifications_for_test(with_ntfy_and_irc("topic"));
        let mut d = dispatcher_for(&cfg, with_ntfy_and_irc("topic"));

        cfg.set_notifications_for_test(NotificationOptions::default());
        d.refresh();

        assert!(d.targets.is_empty(), "{:?}", names(&d));
    }

    /// A scalar change must not touch the backends at all -- retuning the digest
    /// window should not cost an SMTP connection.
    #[test]
    fn changing_a_scalar_leaves_every_backend_in_place() {
        let mut cfg = Config::default_for_test();
        cfg.set_notifications_for_test(with_ntfy_and_irc("topic"));
        let mut d = dispatcher_for(&cfg, with_ntfy_and_irc("topic"));
        for t in &mut d.targets {
            assert!(t.limit.allow());
        }

        cfg.set_notifications_for_test(NotificationOptions {
            digest_seconds: 30,
            on_torrent_added: true,
            ..with_ntfy_and_irc("topic")
        });
        d.refresh();

        assert_eq!(names(&d), vec!["ntfy", "irc"]);
        assert!(d.targets.iter().all(|t| t.limit.used == 1), "nothing should have been rebuilt");
        assert_eq!(d.options.digest_seconds, 30);
        assert!(d.options.on_torrent_added, "the switch itself is what the dispatcher filters on");
    }

    /// Raising the cap must let the next message through rather than restarting
    /// the hour; lowering it must apply at once.
    #[test]
    fn the_rate_cap_changes_without_resetting_the_window() {
        let mut cfg = Config::default_for_test();
        let start = NotificationOptions { max_per_hour: 2, ..with_ntfy_and_irc("topic") };
        cfg.set_notifications_for_test(start.clone());
        let mut d = dispatcher_for(&cfg, start.clone());

        // Spend the whole allowance.
        for _ in 0..2 {
            assert!(d.targets[0].limit.allow());
        }
        assert!(!d.targets[0].limit.allow(), "the cap should be reached");

        cfg.set_notifications_for_test(NotificationOptions { max_per_hour: 5, ..start });
        d.refresh();

        assert_eq!(d.targets[0].limit.used, 2, "the hour carries over");
        assert!(d.targets[0].limit.allow(), "and the raised cap applies immediately");
    }

    /// The two-way transports are deliberately *not* reloadable: their tables
    /// are consumed by the command side at startup as well, and rebuilding only
    /// the notification half would leave the two roles on different tokens.
    #[test]
    fn a_pinned_transport_survives_a_reload_untouched() {
        let mut cfg = Config::default_for_test();
        cfg.set_notifications_for_test(with_ntfy_and_irc("topic"));
        let mut d = dispatcher_for(&cfg, with_ntfy_and_irc("topic"));

        // Stand in for a Telegram/Slack target: section None means pinned.
        d.targets.push(Target {
            backend: Box::new(NtfyBackend::build(&NtfyOptions {
                topic: "pinned".into(),
                token: None,
                events: EventFilter::default(),
            })
            .expect("ntfy backend")),
            filter: EventFilter::default(),
            limit: RateLimit::new(20),
            outbox: Vec::new(),
            section: None,
        });
        assert!(d.targets.last_mut().unwrap().limit.allow());

        cfg.set_notifications_for_test(NotificationOptions::default());
        d.refresh();

        assert_eq!(d.targets.len(), 1, "only the pinned one remains");
        assert_eq!(d.targets[0].limit.used, 1, "and it was never rebuilt");
    }

    #[test]
    fn the_global_event_switches_are_respected() {
        let inherit = EventFilter::default();
        let mut o = NotificationOptions::default();

        // Defaults: failures on, the chatty one off.
        assert!(failure().enabled_by(&o, &inherit));
        assert!(!Event::TorrentAdded("x".into()).enabled_by(&o, &inherit));

        o.on_torrent_added = true;
        o.on_failure = false;
        assert!(Event::TorrentAdded("x".into()).enabled_by(&o, &inherit));
        assert!(!failure().enabled_by(&o, &inherit));
    }

    /// The per-target override, which is the point of `[notifications.<x>.events]`:
    /// ntfy carrying only failures while email takes everything.
    #[test]
    fn a_target_filter_overrides_only_what_it_states() {
        let global = NotificationOptions {
            on_failure: true,
            on_torrent_added: true,
            on_download_finished: true,
            ..NotificationOptions::default()
        };

        let failures_only = EventFilter {
            on_torrent_added: Some(false),
            on_download_finished: Some(false),
            ..EventFilter::default()
        };

        // Stated: suppressed. Unstated: inherited from the global switch.
        assert!(!Event::TorrentAdded("x".into()).enabled_by(&global, &failures_only));
        assert!(!Event::DownloadFinished("x".into()).enabled_by(&global, &failures_only));
        assert!(failure().enabled_by(&global, &failures_only));

        // And a target can turn something *on* that the global has off.
        let global_quiet =
            NotificationOptions { on_torrent_added: false, ..NotificationOptions::default() };
        let chatty = EventFilter { on_torrent_added: Some(true), ..EventFilter::default() };
        assert!(Event::TorrentAdded("x".into()).enabled_by(&global_quiet, &chatty));
    }

    /// A test notification ignores every switch, global and per-target, because
    /// it is what someone runs to find out whether any of this works.
    #[test]
    fn a_test_notification_ignores_every_switch() {
        let all_off = NotificationOptions {
            on_failure: false,
            on_torrent_added: false,
            on_download_finished: false,
            on_disk_low: false,
            daily_summary: false,
            on_start: false,
            ..NotificationOptions::default()
        };
        let deny_all = EventFilter {
            on_failure: Some(false),
            on_torrent_added: Some(false),
            on_download_finished: Some(false),
            on_disk_low: Some(false),
            daily_summary: Some(false),
            on_start: Some(false),
        };
        assert!(Event::Test.enabled_by(&all_off, &deny_all));
    }

    #[test]
    fn the_rate_limit_counts_what_it_suppressed() {
        let mut limit = RateLimit::new(2);
        assert!(limit.allow());
        assert!(limit.allow());
        assert!(!limit.allow());
        assert!(!limit.allow());
        assert_eq!(limit.take_suppressed(), 2);
        // Reported once, then reset.
        assert_eq!(limit.take_suppressed(), 0);
    }

    #[test]
    fn a_disabled_notifier_silently_accepts_events() {
        // The point of the null handle: call sites stay unconditional.
        Notifier::disabled().send(Event::Test);
    }

    #[test]
    fn a_header_cannot_carry_control_characters() {
        assert_eq!(sanitize_header("a\r\nb\tc"), "a  b c");
        assert!(sanitize_header(&"x".repeat(500)).len() <= 200);
    }

    /// End to end against a real socket, so this covers the parts a unit test
    /// cannot: that the URL is built correctly from a bare topic, that the
    /// request actually goes out, and that the title travels as a header while
    /// the body stays exactly what was rendered.
    #[tokio::test]
    async fn the_ntfy_backend_posts_the_message() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await.unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });

        let backend = NtfyBackend::build(&NtfyOptions {
            topic: format!("http://127.0.0.1:{port}/my-topic"),
            token: Some("hunter2".into()),
            events: EventFilter::default(),
        })
        .expect("backend should build from a full URL");

        backend.deliver("irc2torrent: Finished: Some Release", "Finished: Some Release").await.unwrap();

        let request = server.await.unwrap();
        // Header names are compared case-insensitively: reqwest emits them
        // lowercased, and HTTP says that is the same header.
        let lower = request.to_lowercase();

        assert!(request.starts_with("POST /my-topic "), "{request}");
        assert!(lower.contains("authorization: bearer hunter2"), "{request}");

        // The title is who is telling you, the message is what happened.
        //
        // It used to be `render`'s subject, which for a single event is that
        // event's line -- so ntfy showed the release name as the heading and
        // again as the message. Title and message are separate fields here, so
        // the fix is to put the right thing in each rather than blank one out.
        assert!(lower.contains("title: irc2torrent\r\n"), "{request}");
        assert!(!lower.contains("title: irc2torrent:"), "title still repeats the event: {request}");

        // The body is exactly what was rendered -- the title is out of band.
        assert!(request.ends_with("\r\n\r\nFinished: Some Release"), "{request}");
        assert_eq!(request.matches("Finished: Some Release").count(), 1, "{request}");
    }

    /// A bare topic means the public server; only that shape is expanded.
    #[test]
    fn a_bare_topic_becomes_an_ntfy_sh_url() {
        let built = NtfyBackend::build(&NtfyOptions {
            topic: "my-topic".into(),
            token: None,
            events: EventFilter::default(),
        })
        .expect("a bare topic is enough");
        assert_eq!(built.url, "https://ntfy.sh/my-topic");
    }

    /// An empty topic is a misconfiguration, not a reason to post nowhere.
    #[test]
    fn an_empty_topic_disables_ntfy() {
        assert!(NtfyBackend::build(&NtfyOptions {
            topic: "   ".into(),
            token: None,
            events: EventFilter::default(),
        })
        .is_none());
    }

    /// Email must decline rather than half-configure itself, and must never
    /// panic on a bad section.
    #[test]
    fn email_declines_when_it_cannot_be_configured() {
        use crate::config::config::EmailOptions;

        let base = EmailOptions {
            address: "me@gmail.com".into(),
            password: "app-password".into(),
            password_file: None,
            host: None,
            port: None,
            from: None,
            to: None,
            events: EventFilter::default(),
        };

        // No address at all.
        assert!(EmailBackend::build(&EmailOptions { address: "".into(), ..base.clone() }).is_none());
        // No password anywhere.
        assert!(EmailBackend::build(&EmailOptions {
            password: "".into(),
            ..base.clone()
        })
        .is_none());
        // Unknown domain and no explicit host: nothing to connect to.
        assert!(EmailBackend::build(&EmailOptions {
            address: "me@unknown.example".into(),
            ..base.clone()
        })
        .is_none());
        // An unknown domain *with* a host is fine.
        assert!(EmailBackend::build(&EmailOptions {
            address: "me@unknown.example".into(),
            host: Some("smtp.unknown.example".into()),
            ..base.clone()
        })
        .is_some());
        // And the common case works from address plus password alone.
        assert!(EmailBackend::build(&base).is_some());
    }
}
