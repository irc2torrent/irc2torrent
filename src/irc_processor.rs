pub mod irc {
    use std::any::Any;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use futures::prelude::*;
    use irc::client::prelude::*;
    use irc::client::ClientStream;
    use irc::error::Error;
    use irc::proto::Command;
    use log::{error, info, warn};
    use pub_sub::{PubSub, Subscription};
    use regex::Regex;

    use crate::announce::Announce;
    use crate::auth::AuthResult::*;
    use crate::auth::MessageTypes::Announcement;
    use crate::auth::{redact_secrets, Authorization, MessageOrigin};
    use crate::command_processor::commands::{CommandError, CommandProcessor};
    use crate::torrent_processor::torrent::TorrentProcessor;

    /// Reconnect backoff: first retry after this, doubling to RECONNECT_DELAY_MAX.
    /// There is deliberately no retry ceiling -- see `start_listening`.
    const RECONNECT_DELAY_MIN: Duration = Duration::from_secs(3);
    const RECONNECT_DELAY_MAX: Duration = Duration::from_secs(60);
    /// How long a connection must survive before its backoff is treated as
    /// recovered. Without this, a server that accepts us and immediately drops
    /// the stream would reset the delay on every attempt and we would reconnect
    /// in a tight loop -- the delay only helps if a flapping link keeps backing
    /// off like an unreachable one.
    const STABLE_CONNECTION: Duration = Duration::from_secs(60);
    /// Fallback if the config carries no nickname; matches config.rs's default.
    const DEFAULT_NICK: &str = "irc2torrent";
    /// How often to re-check whether the notification owner is online, and to
    /// expire identification checks that never got an answer.
    const PRESENCE_INTERVAL: Duration = Duration::from_secs(60);
    /// How long to wait for a WHOIS reply before refusing the command it guards.
    /// Failing closed: an unanswered check is not permission.
    const WHOIS_TIMEOUT: Duration = Duration::from_secs(15);
    /// How often expired identity checks are swept.
    ///
    /// Its own ticker rather than `PRESENCE_INTERVAL`'s: expiry used to ride the
    /// 60s presence tick, so a 15s timeout was really anywhere from 15s to 75s
    /// and the refusal arrived long after the sender had given up.
    const AUTH_SWEEP_INTERVAL: Duration = Duration::from_secs(5);
    /// Consecutive failed identity checks before a nick stops being asked about.
    const WHOIS_FAILURE_LIMIT: u8 = 3;
    /// How long a nick that keeps failing is left alone.
    ///
    /// Long enough to defeat a loop, short enough that an owner who simply
    /// forgot to identify to NickServ is not locked out for the afternoon.
    const WHOIS_COOLDOWN: Duration = Duration::from_secs(600);
    /// Ceiling on identity checks per minute, whatever the nick.
    ///
    /// The per-nick gate and cooldown already bound this to whoever holds the
    /// owner's nick; this is the backstop that holds when both are somehow
    /// wrong, so the bot cannot be turned into a WHOIS amplifier at all.
    const WHOIS_PER_MINUTE: u32 = 10;
    /// Commands one nick may have queued behind a single identity check.
    ///
    /// Was unbounded, and each queued command drew its own refusal -- so a
    /// stranger sending `h!` a thousand times had the bot queue a thousand
    /// replies and spend half an hour draining them at the flood limit. The
    /// bot was the thing being flooded, and it needed no WHOIS to do it.
    const PENDING_COMMAND_LIMIT: usize = 5;
    /// `330 <me> <nick> <account> :is logged in as`. Not in irc-proto's Response
    /// enum, so it arrives as a raw numeric.
    const RPL_WHOISACCOUNT: &str = "330";

    /// A command held while its sender's identification is verified.
    ///
    /// Verification is a WHOIS round trip, and the reply arrives on the same
    /// stream this loop is reading -- so the command cannot simply await it
    /// without deadlocking. It is parked here and run when 318 closes the WHOIS.
    struct PendingAuth {
        /// `(target, message)` in arrival order; a second command before the
        /// first resolves joins the same WHOIS rather than starting another.
        ///
        /// The *target* is stored, not the reply address. Authorization asks
        /// whether the message was a private message, which it decides by
        /// comparing the target with our own nick -- so replacing it with the
        /// reply address here makes every queued command look like a channel
        /// message and fail authorization after passing the identity check.
        commands: Vec<(String, String)>,
        /// How many were turned away because `commands` was full. Reported once,
        /// with the single reply, rather than one message per dropped command.
        dropped: usize,
        /// Account name from a 330, if the network sent one.
        account: Option<String>,
        deadline: Instant,
    }

    /// How a nick has been faring at the identity check.
    ///
    /// Only failures are remembered. A *successful* check is deliberately not
    /// cached: the whole reason `require_identified` exists is that a nick can
    /// be taken the moment its owner drops off, and holding "this nick is the
    /// owner" for any length of time reopens exactly that window. Refusing
    /// someone who was just refused is fail-closed; trusting someone who was
    /// trusted a while ago is not.
    #[derive(Default)]
    struct WhoisCooldown {
        /// Consecutive failures. Reset by a check that succeeds.
        failures: u8,
        /// When lookups for this nick may resume.
        until: Option<Instant>,
    }

    /// Per-nick record of failed identity checks.
    ///
    /// Takes `now` rather than reading the clock, so the cooldown can be tested
    /// without a test that sleeps for ten minutes.
    #[derive(Default)]
    struct WhoisCooldowns(HashMap<String, WhoisCooldown>);

    impl WhoisCooldowns {
        /// Whether this nick is currently being left alone.
        fn is_cooling(&self, nick: &str, now: Instant) -> bool {
            self.0
                .get(&nick.to_ascii_lowercase())
                .and_then(|c| c.until)
                .is_some_and(|until| until > now)
        }

        /// Record a failure. True when this is the one that starts a cooldown,
        /// which is the only time the sender is told about it.
        fn note_failure(&mut self, nick: &str, now: Instant) -> bool {
            let entry = self.0.entry(nick.to_ascii_lowercase()).or_default();
            entry.failures = entry.failures.saturating_add(1);
            if entry.failures < WHOIS_FAILURE_LIMIT {
                return false;
            }
            // Counted back down, so a nick that keeps trying after the cooldown
            // expires earns another one rather than being locked out forever.
            entry.failures = 0;
            entry.until = Some(now + WHOIS_COOLDOWN);
            true
        }

        /// Forget a nick's failures after a check it passed.
        fn note_success(&mut self, nick: &str) {
            self.0.remove(&nick.to_ascii_lowercase());
        }

        /// Drop entries that are neither cooling nor part-way to a cooldown.
        fn prune(&mut self, now: Instant) {
            self.0.retain(|_, c| c.failures > 0 || c.until.is_some_and(|u| u > now));
        }
    }

    /// Token bucket over a rolling minute, shared by every nick.
    struct WhoisBudget {
        window_started: Instant,
        used: u32,
    }

    impl WhoisBudget {
        fn new(now: Instant) -> Self {
            Self { window_started: now, used: 0 }
        }

        /// Spend one, or report that this minute is used up.
        fn take(&mut self, now: Instant) -> bool {
            if now.duration_since(self.window_started) >= Duration::from_secs(60) {
                self.window_started = now;
                self.used = 0;
            }
            if self.used >= WHOIS_PER_MINUTE {
                return false;
            }
            self.used += 1;
            true
        }
    }

    /// Strip anything that could end the IRC line early.
    ///
    /// A PRIVMSG is terminated by CRLF, so a newline inside the payload ends the
    /// line and the server reads the remainder as a fresh command. Some of what
    /// the bot echoes back is attacker controlled -- a release name from an
    /// announce, or a torrent name out of `.torrent` metadata reaching us through
    /// `torrentlist` -- so a name containing "\r\nJOIN #somewhere" would
    /// otherwise be command injection carrying the bot's own privileges.
    ///
    /// NUL goes too: it terminates the line for some servers and C-based clients.
    ///
    /// Applied at the single send site rather than per caller, so every reply is
    /// covered and a new one cannot forget.
    pub(crate) fn sanitize_for_irc(message: &str) -> String {
        message
            .chars()
            .map(|c| if c == '\r' || c == '\n' || c == '\0' { ' ' } else { c })
            .collect()
    }

    /// One queued outbound message.
    ///
    /// Carries the cancellation epoch it was queued under, which is what makes
    /// `stop!` work: the pacer discards anything stamped with a superseded epoch
    /// rather than sending it.
    pub struct Outgoing {
        target: String,
        text: String,
        epoch: u64,
        /// Whether `stop!` may discard this.
        ///
        /// Command replies are; notifications are not. They share the queue
        /// because they share the connection, but "stop listing torrents at me"
        /// should not silently throw away a download-finished alert that
        /// happened to be waiting behind it.
        cancellable: bool,
    }

    impl Outgoing {
        #[cfg(test)]
        pub(crate) fn text(&self) -> &str {
            &self.text
        }
    }

    /// Handle for queuing outbound messages, and for cancelling what is queued.
    ///
    /// Cloneable and `Send`, so command replies, notifications and the pacer all
    /// share one queue -- they share a connection, so they must share both its
    /// rate budget and its cancellation.
    #[derive(Clone)]
    pub struct Outbound {
        tx: tokio::sync::mpsc::UnboundedSender<Outgoing>,
        /// Bumped by `cancel`; anything queued under an older value is dropped.
        ///
        /// An epoch rather than draining the channel, because the pacer is
        /// usually *asleep* between messages. It could not act on a "clear"
        /// signal until it woke, and making the sleep interruptible would mean
        /// `select!`ing on the receiver and popping a message it is not yet
        /// ready to send. Comparing on pop needs nothing to be interrupted.
        epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
        /// Queued but not yet handled, so `stop` can say what it discarded.
        pending: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Outbound {
        /// Queue a command reply. Never blocks; the pacer decides when it leaves.
        /// Discardable by `stop!`.
        pub fn send(&self, target: &str, text: &str) {
            self.queue(target, text, true);
        }

        /// Queue something `stop!` must not throw away, such as a notification.
        pub fn send_uninterruptible(&self, target: &str, text: &str) {
            self.queue(target, text, false);
        }

        fn queue(&self, target: &str, text: &str, cancellable: bool) {
            use std::sync::atomic::Ordering;

            let msg = Outgoing {
                target: target.to_string(),
                text: text.to_string(),
                epoch: self.epoch.load(Ordering::Relaxed),
                cancellable,
            };
            if cancellable {
                self.pending.fetch_add(1, Ordering::Relaxed);
            }
            if self.tx.send(msg).is_err() {
                if cancellable {
                    self.pending.fetch_sub(1, Ordering::Relaxed);
                }
                error!("Outbound queue is gone; dropping message to {target}.");
            }
        }

        /// Whether the pacer should drop this rather than send it.
        ///
        /// Split out so the cancellation policy can be tested without a live
        /// connection; the pacer applies it twice, once on pop and once after
        /// any wait.
        pub(crate) fn discards(msg: &Outgoing, current_epoch: u64) -> bool {
            msg.cancellable && msg.epoch < current_epoch
        }

        /// The queue plus its receiver, for tests that need to inspect what was
        /// stamped without standing up an IRC connection.
        #[cfg(test)]
        pub(crate) fn for_test() -> (Self, tokio::sync::mpsc::UnboundedReceiver<Outgoing>) {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            (
                Self {
                    tx,
                    epoch: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    pending: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                },
                rx,
            )
        }

        #[cfg(test)]
        pub(crate) fn current_epoch(&self) -> u64 {
            self.epoch.load(std::sync::atomic::Ordering::Relaxed)
        }

        /// Discard every cancellable message queued, returning roughly how many.
        ///
        /// One epoch for the whole queue, so a `stop!` clears *all* outstanding
        /// replies rather than only the most recent request -- if two listings
        /// are still draining, "stop" plainly means both. Notifications are
        /// untouched.
        ///
        /// The count is approximate by nature: the pacer may be mid-message as
        /// this runs. It is a figure for a human, not a guarantee.
        pub fn cancel(&self) -> usize {
            use std::sync::atomic::Ordering;

            self.epoch.fetch_add(1, Ordering::Relaxed);
            self.pending.swap(0, Ordering::Relaxed)
        }
    }

    /// Pace outbound PRIVMSGs, returning the queue to push them onto.
    ///
    /// The `irc` crate advertises `burst_window_length` and
    /// `max_messages_in_burst` on its Config and even provides getters for them
    /// -- but version 1.1.0 never reads either one, and contains no throttling
    /// code whatsoever. The settings are documentation for behaviour that does
    /// not exist, which is why a multi-line reply went out as fast as the socket
    /// would take it and got the bot killed for excess flood.
    ///
    /// So the limit is enforced here. A token bucket rather than a fixed delay:
    /// up to `max_in_burst` messages may leave immediately, and only once that
    /// many have gone out inside `window` does the next one wait. A one-line
    /// reply is therefore still instant.
    ///
    /// Runs as its own task because it sleeps. Doing this inline would stall the
    /// read loop, and a client that stops reading stops answering server PINGs.
    fn spawn_pacer(sender: Sender, limit: crate::config::config::SharedFloodLimit) -> Outbound {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Outgoing>();
        let epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let pending = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (task_epoch, task_pending) = (epoch.clone(), pending.clone());

        tokio::spawn(async move {
            let (epoch, pending) = (task_epoch, task_pending);
            let mut recent: std::collections::VecDeque<tokio::time::Instant> =
                std::collections::VecDeque::new();

            while let Some(msg) = rx.recv().await {
                if msg.cancellable {
                    pending.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }

                // Cancelled before it ever got a turn. Dropped without waiting
                // and without consuming a rate slot, so a `stop!` clears a long
                // backlog in milliseconds rather than draining it slowly.
                if Outbound::discards(&msg, epoch.load(std::sync::atomic::Ordering::Relaxed)) {
                    continue;
                }

                // Read the limit per message rather than capturing it, so an
                // edit to irc.toml applies to the very next send instead of
                // waiting for a reconnect. Cheap: an uncontended lock.
                let (window, max_in_burst) = match limit.lock() {
                    Ok(l) => (l.window, l.max_in_burst),
                    Err(e) => {
                        error!("Flood limit lock poisoned: {e}");
                        return;
                    }
                };

                // Forget anything that has aged out of the window.
                while recent.front().is_some_and(|t| t.elapsed() >= window) {
                    recent.pop_front();
                }

                if recent.len() >= max_in_burst {
                    // The oldest send still inside the window decides how long
                    // until a slot frees up.
                    if let Some(oldest) = recent.front().copied() {
                        let wait = window.saturating_sub(oldest.elapsed());
                        if !wait.is_zero() {
                            tokio::time::sleep(wait).await;
                        }
                        recent.pop_front();
                    }
                }

                // Checked again after the wait: `stop!` most often arrives while
                // the pacer is sleeping, and without this the message it was
                // sleeping for would still go out.
                if Outbound::discards(&msg, epoch.load(std::sync::atomic::Ordering::Relaxed)) {
                    continue;
                }

                recent.push_back(tokio::time::Instant::now());
                if let Err(e) = sender.send_privmsg(&msg.target, &msg.text) {
                    // The connection has gone; the reconnect loop will replace
                    // this pacer along with the sender it holds.
                    error!("Could not send message to {}: {e:?}", msg.target);
                    return;
                }
            }
        });

        Outbound { tx, epoch, pending }
    }

    pub struct IrcProcessor {
        evt_channel: PubSub<String>,
        subs_cfg: Vec<Subscription<String>>,
        config: Rc<RefCell<crate::config::config::Config>>,
        tp: Rc<TorrentProcessor>,
        cp: Rc<CommandProcessor>,
        client: Rc<RefCell<Option<Client>>>,
        status_response_regex: Regex,
        auth: Authorization,
        user_status: HashMap<String, UserStatus>,
        our_nick: String,
        notifier: crate::notify::Notifier,
        /// Republished on every connect so the notification backend never holds
        /// a sender from a connection that has since dropped.
        sender_slot: crate::notify::IrcSenderSlot,
        /// Commands waiting on a WHOIS, keyed by lowercased sender nick.
        pending_auth: HashMap<String, PendingAuth>,
        /// Failed identity checks per nick, and when to resume asking.
        whois_cooldown: WhoisCooldowns,
        /// Global ceiling on identity checks, whatever the nick.
        whois_budget: WhoisBudget,
        /// Paced outbound queue for the current connection, replaced on each
        /// reconnect along with the sender it wraps.
        outbound: Option<Outbound>,
    }

    #[derive(Debug)]
    pub struct UserStatus {
        nick: String,
        status: u8,
        time_of_check: u64,
    }

    impl IrcProcessor {
        pub fn new(cfg: Rc<RefCell<crate::config::config::Config>>, torrent_processor: Rc<TorrentProcessor>, command_processor: Rc<CommandProcessor>, evt_channel: PubSub<String>, subs_cfg: Vec<Subscription<String>>, notifier: crate::notify::Notifier, sender_slot: crate::notify::IrcSenderSlot) -> Self {
            let our_nick = cfg
                .borrow()
                .get_irc_config()
                .nickname
                .clone()
                .unwrap_or_else(|| DEFAULT_NICK.to_string());

            Self {
                config: cfg.clone(),
                tp: torrent_processor,
                cp: command_processor,
                evt_channel,
                subs_cfg,
                client: Rc::new(RefCell::new(None)),
                status_response_regex: Regex::new(r"STATUS (?P<nick>\S+) (?P<status>\d)").unwrap(),
                auth: Authorization::new(cfg.clone()),
                user_status: HashMap::new(),
                our_nick,
                notifier,
                sender_slot,
                pending_auth: HashMap::new(),
                whois_cooldown: WhoisCooldowns::default(),
                whois_budget: WhoisBudget::new(Instant::now()),
                outbound: None,
            }
        }

        pub async fn start_listening(&mut self) {
            let mut retry_count: u32 = 0;
            let mut backoff = RECONNECT_DELAY_MIN;
            // When the link first went down, and whether that has been reported.
            // A brief blip on a reconnecting bot is normal and not worth a
            // notification; a link that stays down is the thing worth knowing.
            let mut down_since: Option<Instant> = None;
            let mut reported_down = false;

            loop {
                if let Some(mut stream) = self.connect_irc().await {
                    if reported_down {
                        let down_for =
                            down_since.map(|t| t.elapsed().as_secs()).unwrap_or_default();
                        info!("IRC recovered after {down_for}s.");
                        // Only because the outage was reported: leaving an "IRC
                        // is down" message with no all-clear is worse than never
                        // having sent it.
                        self.notifier.send(crate::notify::Event::IrcReconnected(down_for));
                    }
                    down_since = None;
                    reported_down = false;
                    let connected_at = Instant::now();
                    self.probe_owner_presence();

                    // A timer runs beside the stream so presence stays current
                    // and an unanswered WHOIS cannot hold a command forever.
                    // Both arm bodies are non-blocking -- nothing here may await
                    // for long, or the client stops answering server PINGs.
                    let mut ticker = tokio::time::interval(PRESENCE_INTERVAL);
                    ticker.tick().await;
                    // Expiry gets its own, much shorter tick. Riding the presence
                    // interval meant a 15s timeout fired anywhere between 15s and
                    // 75s after the WHOIS, so the refusal reached the sender long
                    // after they had concluded the bot was ignoring them.
                    let mut sweep = tokio::time::interval(AUTH_SWEEP_INTERVAL);
                    sweep.tick().await;

                    loop {
                        tokio::select! {
                            received = stream.next() => match received.transpose() {
                                Ok(Some(msg)) => {
                                    self.msg_process(&msg).await;
                                }
                                Ok(None) => {
                                    warn!("IRC stream ended.");
                                    break;
                                }
                                Err(e) => {
                                    if e.type_id() == Error::PingTimeout.type_id() {
                                        warn!("Ping timeout.");
                                    } else {
                                        error!("IRC stream error: {:?}", e);
                                    }
                                    break;
                                }
                            },
                            _ = ticker.tick() => {
                                self.probe_owner_presence();
                            }
                            _ = sweep.tick() => {
                                self.expire_pending_auth();
                            }
                        }
                    }

                    // The connection is gone: nothing queued on it can be
                    // answered, and the owner's presence is no longer known.
                    self.drop_pending_auth("the connection to IRC dropped");
                    if let Ok(mut link) = self.sender_slot.lock() {
                        link.owner_online = false;
                    }

                    // Only a connection that actually held counts as recovery.
                    // Previously any successful connect reset the counter and the
                    // stream-error path reconnected with no delay at all, so a
                    // server accepting and instantly dropping us produced a tight
                    // loop rather than a backing-off one.
                    let uptime = connected_at.elapsed();
                    if uptime >= STABLE_CONNECTION {
                        retry_count = 0;
                        backoff = RECONNECT_DELAY_MIN;
                    } else {
                        warn!("Connection lasted only {}s; backing off.", uptime.as_secs());
                    }
                }
                // No `else` logging the failure: `connect_irc` already reported
                // it with the underlying error, and saying it again without one
                // just doubled every connection failure in the log.

                // Report once the outage has lasted past a full backoff cap, so
                // a routine reconnect stays quiet but a real outage does not.
                let down_for = down_since.get_or_insert_with(Instant::now).elapsed();
                if !reported_down && down_for >= RECONNECT_DELAY_MAX {
                    reported_down = true;
                    self.notifier
                        .send(crate::notify::Event::IrcDisconnected(down_for.as_secs()));
                }

                // Retry indefinitely rather than giving up after a fixed count.
                // A seedbox bot that quits during a network outage is useless
                // precisely when it needs to recover on its own, and with the
                // delay capped at a minute this costs the server one connection
                // attempt per minute.
                retry_count = retry_count.saturating_add(1);

                info!(
                    "Reconnecting in {}s (attempt {}).",
                    backoff.as_secs(),
                    retry_count
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RECONNECT_DELAY_MAX);
            }
        }

        /// Ask the server whether the notification owner is on the network.
        ///
        /// ISON rather than WHOIS: it is the purpose-built check, one line each
        /// way, and cheap enough to repeat every minute.
        fn probe_owner_presence(&self) {
            let crate::config::config::SecurityMode::IrcUserName(owner) =
                self.config.borrow().get_security_mode()
            else {
                return;
            };
            if let Some(c) = self.client.borrow_mut().as_mut() {
                let _ = c.send(Command::ISON(vec![owner]));
            }
        }

        /// Refuse commands whose WHOIS never came back.
        ///
        /// Fails closed: an unanswered identity check is not permission.
        fn expire_pending_auth(&mut self) {
            let now = Instant::now();
            let expired: Vec<String> = self
                .pending_auth
                .iter()
                .filter(|(_, p)| p.deadline <= now)
                .map(|(nick, _)| nick.clone())
                .collect();

            for nick in expired {
                if let Some(p) = self.pending_auth.remove(&nick) {
                    warn!("No WHOIS reply for {nick}; refusing {} command(s).", p.commands.len());
                    // An unanswered check is a failed one for cooldown purposes:
                    // a network that does not answer will not answer the next
                    // one either, and this is exactly the shape a flood takes.
                    self.refuse_pending(
                        &nick,
                        &p,
                        "Could not verify your identity with the network in time; command \
                         refused. If your network has no services, set \
                         command_options.require_identified = false."
                            .to_string(),
                    );
                }
            }

            // Cooldowns that have run their course, so the map does not keep an
            // entry per nick that ever failed.
            self.whois_cooldown.prune(now);
        }

        /// Abandon everything queued on a connection that has gone away.
        fn drop_pending_auth(&mut self, why: &str) {
            for (nick, p) in std::mem::take(&mut self.pending_auth) {
                warn!("Dropping {} queued command(s) from {nick}: {why}.", p.commands.len());
            }
        }

        /// Run or refuse everything queued for `nick`, now that its WHOIS closed.
        async fn resolve_pending_auth(&mut self, nick: &str) {
            let Some(pending) = self.pending_auth.remove(&nick.to_ascii_lowercase()) else {
                return;
            };

            // The account must match the configured owner, not merely be
            // present. On a network that does not enforce nick ownership an
            // impostor can hold the nick while identified to their *own*
            // account, which a bare "is identified" check would wave through.
            let expected = match self.config.borrow().get_security_mode() {
                crate::config::config::SecurityMode::IrcUserName(n) => n,
                crate::config::config::SecurityMode::Password(_) => return,
            };

            // Lifted out of `pending` before the match so the arms are free to
            // consume it: one of them moves the queued commands, the others
            // hand the whole thing to `refuse_pending`.
            let account = pending.account.clone();

            match account.as_deref() {
                Some(account) if account.eq_ignore_ascii_case(&expected) => {
                    info!("{nick} is identified as {account}; running queued command(s).");
                    // Only failures are remembered, and this was not one.
                    self.note_whois_success(nick);
                    for (target, message) in pending.commands {
                        self.run_command(&target, &message, nick).await;
                    }
                }
                Some(account) => {
                    error!("{nick} is identified as '{account}', not '{expected}'; refusing.");
                    let reason = format!(
                        "You are logged in as '{account}', but this bot only accepts \
                         '{expected}'. Set command_options.security_mode to your services \
                         account name."
                    );
                    self.refuse_pending(nick, &pending, reason);
                }
                None => {
                    error!("{nick} is not identified to services; refusing command(s).");
                    self.refuse_pending(
                        nick,
                        &pending,
                        "You are not identified to network services. Identify with NickServ \
                         and try again."
                            .to_string(),
                    );
                }
            }
        }

        /// Answer a batch of refused commands with a single message.
        ///
        /// One, not one each. A reply per queued entry is how a stranger got the
        /// bot to send a message for every message they sent it -- and with the
        /// queue previously unbounded, to spend half an hour draining them at
        /// the flood limit while everything else waited behind.
        fn refuse_pending(&mut self, nick: &str, pending: &PendingAuth, reason: String) {
            // Answer where the first command came from; the rest were queued
            // behind it and share its outcome.
            let Some((target, _)) = pending.commands.first() else {
                return;
            };
            let to = self.reply_to(target, nick).to_string();

            let mut reason = reason;
            if pending.dropped > 0 {
                reason.push_str(&format!(
                    " ({} further command(s) went unread.)",
                    pending.dropped
                ));
            }
            self.send_privmsg(&to, &reason);

            // Sent once, when the cooldown starts. Afterwards the nick is
            // ignored in silence.
            if let Some(notice) = self.note_whois_failure(nick) {
                self.send_privmsg(&to, &notice);
            }
        }

        async fn msg_process(&mut self, msg: &Message) {
            // `target` is where the message was addressed: a channel, or our own
            // nick for a private message. It is NOT the sender -- conflating the
            // two previously made private-message auth and the NickServ branch
            // both unreachable.
            match &msg.command {
                // `330 <me> <nick> <account> :is logged in as`. Not in
                // irc-proto's Response enum, so it arrives raw.
                Command::Raw(code, args) if code == RPL_WHOISACCOUNT && args.len() >= 3 => {
                    let (nick, account) = (&args[1], &args[2]);
                    if let Some(p) = self.pending_auth.get_mut(&nick.to_ascii_lowercase()) {
                        p.account = Some(account.clone());
                    }
                    return;
                }
                // End of WHOIS: whatever we learned is all we are going to.
                Command::Response(Response::RPL_ENDOFWHOIS, args) if args.len() >= 2 => {
                    let nick = args[1].clone();
                    self.resolve_pending_auth(&nick).await;
                    return;
                }
                // `303 <me> :<space-separated nicks that are online>`
                Command::Response(Response::RPL_ISON, args) => {
                    let online = args.last().map(String::as_str).unwrap_or("");
                    self.note_owner_presence(online);
                    return;
                }
                _ => {}
            }

            let (Command::PRIVMSG(target, inner_message), Some(sender)) =
                (&msg.command, msg.source_nickname())
            else {
                return;
            };

            // Never log a message verbatim: in password security mode the
            // credential travels inside it as "auth:[...]".
            info!("{}@{}: {}", sender, target, redact_secrets(inner_message));

            if sender.eq_ignore_ascii_case("NickServ") {
                self.nickserv_msg_process(inner_message);
                return;
            }

            // Commands are checked first, and the order is load-bearing.
            //
            // `cmd:addtorrent` takes an announce line as its parameter, so its
            // text necessarily matches the announce regex too. Testing the
            // announce regex first therefore routed every addtorrent command to
            // the announcement handler, which rejects anything not arriving on a
            // configured channel -- so the command was unreachable, and a
            // private message asking for it reported only "Announcement from an
            // unconfigured source ignored".
            //
            // A command carries an explicit `cmd:` marker while an announcement
            // is merely pattern-matched, so the explicit marker wins.
            if self.cp.is_command(inner_message) {
                self.command_msg_process(target, inner_message, sender).await;
                return;
            }

            // Both in one borrow, and bound to locals so the RefCell guard is
            // dropped before the await below.
            let (announce_regex, capture_options) = {
                let cfg = self.config.borrow();
                (cfg.get_announce_regex(), cfg.get_capture_options())
            };
            if let Some(caps) = announce_regex.captures(inner_message) {
                match Announce::from_captures(&announce_regex, &caps, &capture_options) {
                    Some(announce) => self.torrent_msg_process(target, sender, &announce).await,
                    // Config validation requires both groups, so getting here
                    // means they are declared inside an alternation that did not
                    // fire. Previously this indexed the captures directly and
                    // panicked in the read loop.
                    None => warn!(
                        "Announce line matched but `name` or `id` did not capture; ignoring it. \
                         Check for an alternation in regex_for_announce_match."
                    ),
                }
            } else {
                info!("Message is not a torrent or a command.");
            }
        }

        fn nickserv_msg_process(&mut self, inner_message: &str) {
            if !inner_message.contains("STATUS") {
                return;
            }
            // Previously `.captures(..).unwrap()` and `parse().unwrap()`, either of
            // which panicked on any NickServ message that merely mentioned STATUS.
            let Some(caps) = self.status_response_regex.captures(inner_message) else {
                warn!("Unrecognised NickServ STATUS response.");
                return;
            };
            let Ok(status) = caps["status"].parse::<u8>() else {
                warn!("Unparseable NickServ status value.");
                return;
            };
            let nick = caps["nick"].to_string();
            self.user_status_report(nick.as_str(), status);
        }

        /// Record whether the owner appeared in an ISON reply.
        fn note_owner_presence(&self, online_list: &str) {
            let crate::config::config::SecurityMode::IrcUserName(owner) =
                self.config.borrow().get_security_mode()
            else {
                return;
            };
            let online = online_list.split_whitespace().any(|n| n.eq_ignore_ascii_case(&owner));

            if let Ok(mut link) = self.sender_slot.lock() {
                if link.owner_online != online {
                    info!("{owner} is now {}.", if online { "online" } else { "offline" });
                }
                link.owner_online = online;
            }
        }

        async fn command_msg_process(&mut self, target: &str, inner_message: &str, sender: &str) {
            let direct = target.eq_ignore_ascii_case(&self.our_nick);

            // Settle what this message could possibly achieve before spending
            // anything on it. Routing here is purely syntactic -- `is_command`
            // matches `cmd:` and the short forms and nothing else -- so without
            // this, a stranger typing `h!` in the announce channel bought a
            // WHOIS and a reply naming the owner's account.
            match self.auth.gate_irc_command(sender, direct) {
                crate::auth::CommandGate::Proceed => {}
                crate::auth::CommandGate::Refuse(why) => {
                    info!("Command from {sender} refused: {why}");
                    let to = self.reply_to(target, sender).to_string();
                    self.send_privmsg(&to, why);
                    return;
                }
                crate::auth::CommandGate::Ignore => {
                    info!(
                        "Ignoring a command-shaped message from {sender} on {target}: it could \
                         not be authorized however it resolved, so it costs neither an identity \
                         check nor a reply."
                    );
                    return;
                }
            }

            info!("Message is a command from {sender}.");

            // Verify identity with the network before running anything, if asked
            // to. A nickname on its own is not a credential: on a network that
            // does not enforce registration, anyone can take yours the moment
            // you drop off and command the bot as you.
            //
            // The WHOIS reply arrives on the stream this loop is reading, so the
            // command cannot await it here without deadlocking -- it is parked
            // and run when 318 closes the WHOIS.
            let gated = self.config.borrow().requires_identified()
                && matches!(
                    self.config.borrow().get_security_mode(),
                    crate::config::config::SecurityMode::IrcUserName(_)
                );

            if gated {
                let key = sender.to_ascii_lowercase();

                // A nick that has failed the check repeatedly is not asked about
                // again for a while. Silent by design: the message saying so was
                // already sent when the cooldown began, and repeating it per
                // attempt would restore the amplifier this is here to remove.
                let now = Instant::now();
                if self.whois_cooldown.is_cooling(sender, now) {
                    info!(
                        "{sender} failed the identity check {WHOIS_FAILURE_LIMIT} times; \
                         ignoring commands from that nick until the cooldown expires."
                    );
                    return;
                }

                // A second command arriving before the first resolves rides the
                // WHOIS already in flight rather than starting another, and so
                // is not charged to the budget either.
                let in_flight = self.pending_auth.contains_key(&key);
                if !in_flight && !self.whois_budget.take(now) {
                    warn!("WHOIS budget for this minute is spent; refusing {sender}'s command.");
                    let to = self.reply_to(target, sender).to_string();
                    self.send_privmsg(&to, "Too many identity checks just now; try again shortly.");
                    return;
                }

                let entry = self.pending_auth.entry(key).or_insert_with(|| PendingAuth {
                    commands: Vec::new(),
                    dropped: 0,
                    account: None,
                    deadline: Instant::now() + WHOIS_TIMEOUT,
                });

                // Keep the first few and count the rest. The earliest commands
                // are the ones that were meant; a flood behind them is not.
                if entry.commands.len() < PENDING_COMMAND_LIMIT {
                    entry.commands.push((target.to_string(), inner_message.to_string()));
                } else {
                    entry.dropped = entry.dropped.saturating_add(1);
                }

                if !in_flight {
                    if let Some(c) = self.client.borrow_mut().as_mut() {
                        if let Err(e) = c.send(Command::WHOIS(None, sender.to_string())) {
                            error!("Could not send WHOIS for {sender}: {e:?}");
                        }
                    }
                }
                return;
            }

            self.run_command(target, inner_message, sender).await;
        }

        /// Record a failed identity check, and start a cooldown once a nick has
        /// managed enough of them.
        ///
        /// Returns the message to send, if this failure is the one that begins
        /// the cooldown -- so the sender is told once, at the point it starts,
        /// and not on every attempt afterwards.
        fn note_whois_failure(&mut self, nick: &str) -> Option<String> {
            if !self.whois_cooldown.note_failure(nick, Instant::now()) {
                return None;
            }

            warn!(
                "{nick} has failed the identity check {WHOIS_FAILURE_LIMIT} times; ignoring \
                 commands from that nick for {}s.",
                WHOIS_COOLDOWN.as_secs()
            );
            Some(format!(
                "That is {WHOIS_FAILURE_LIMIT} failed identity checks, so I will ignore commands \
                 from this nick for {} minutes. If your network has no services, set \
                 command_options.require_identified = false.",
                WHOIS_COOLDOWN.as_secs() / 60
            ))
        }

        /// Forget a nick's failures after a check it passed.
        fn note_whois_success(&mut self, nick: &str) {
            self.whois_cooldown.note_success(nick);
        }

        /// Execute a command that has cleared the identity check, if one applied.
        ///
        /// Takes the original `target`, not the reply address: authorization
        /// decides "is this a private message?" by comparing the target with our
        /// own nick, so handing it the reply address would make every private
        /// command look like a channel one and be refused.
        ///
        /// Authorization proper still happens inside `process_command`; the
        /// identity check only establishes *who* is asking, not what they may do.
        async fn run_command(&mut self, target: &str, inner_message: &str, sender: &str) {
            let request = crate::auth::CommandRequest {
                principal: crate::auth::Principal::Irc { nick: sender.to_string() },
                // Authorization asks whether this was private, which for IRC
                // means the target is our own nick rather than a channel.
                direct: target.eq_ignore_ascii_case(&self.our_nick),
                text: inner_message.to_string(),
            };
            let reply_to = self.reply_to(target, sender).to_string();
            let reply_to = reply_to.as_str();

            // Authorization is enforced inside process_command.
            match self.cp.process_command(&request).await {
                Ok(lines) => {
                    // One PRIVMSG per line. IRC has no multi-line message, and
                    // packing a listing into one caps it at ~380 bytes -- which
                    // is where most of a real torrent list was being lost. The
                    // client's own send queue paces these, and the command
                    // layer bounds how many there can be.
                    info!("Command result: {} line(s)", lines.len());
                    for line in lines {
                        self.send_privmsg(reply_to, &line);
                    }
                }
                Err(CommandError::Unauthorized) => {
                    error!("{sender} is not authorized to use this bot.");
                    self.send_privmsg(reply_to, &CommandError::Unauthorized.to_string());
                }
                Err(e) => {
                    error!("Command failed: {e}");
                    self.send_privmsg(reply_to, &e.to_string());
                }
            }
        }

        async fn torrent_msg_process(&mut self, target: &str, sender: &str, announce: &Announce) {
            info!("Torrent name: {}", announce.name);
            info!("Torrent Id: {}", announce.id);
            let extra: Vec<String> = announce
                .field_names()
                .map(|f| match announce.get(f) {
                    Some("") | None => f.to_string(),
                    Some(v) => format!("{f}={v}"),
                })
                .collect();
            if !extra.is_empty() {
                info!("Torrent fields: {}", extra.join(", "));
            }

            let origin = MessageOrigin::new(sender, target, &self.our_nick);
            if let SourceValidated = self.auth.authenticate(&origin, "", Announcement) {
                let outcome = self.tp.process_torrent(announce).await;
                // reply_to, not target: for a private message the target is our
                // own nick, so replying there made the bot message itself and
                // then process its own reply.
                self.send_privmsg(self.reply_to(target, sender), &outcome.to_string());
            } else {
                info!(
                    "Ignoring announcement from {sender} on {target}: announcements are accepted \
                     on a configured channel, or by private message from the owner when \
                     commands_enabled is set."
                );
            }
        }

        /// Where a reply to this message should go.
        ///
        /// A PRIVMSG's target is a channel for channel messages but *our own
        /// nick* for a private message, so replying to the target verbatim makes
        /// the bot message itself -- and then process its own reply as an
        /// incoming message.
        fn reply_to<'a>(&self, target: &'a str, sender: &'a str) -> &'a str {
            if target.eq_ignore_ascii_case(&self.our_nick) {
                sender
            } else {
                target
            }
        }

        pub fn user_status_report(&mut self, nick: &str, status: u8) {
            let time = chrono::Utc::now().timestamp();
            let user = UserStatus { nick: nick.to_string(), status, time_of_check: time as u64 };
            info!("User status report: {user:?}");
            self.user_status.insert(nick.to_string(), user);
        }

        pub fn update_user_status(&self, nick: &str) {
            if let Some(c) = self.client.borrow_mut().as_mut() {
                let _ = c.send_privmsg("NickServ", format!("STATUS {}", nick));
            }
        }

        /// Queue a reply. Never blocks: the pacer decides when it leaves.
        fn send_privmsg(&self, target: &str, message: &str) {
            let message = sanitize_for_irc(message);
            let Some(outbound) = &self.outbound else {
                error!("Not connected; dropping message to {target}.");
                return;
            };
            outbound.send(target, &message);
        }

        pub async fn connect_irc(&mut self) -> Option<ClientStream> {
            let cfg = self.config.borrow().get_irc_config();
            // Shared, not copied: the pacer re-reads it per message so the
            // burst settings can be retuned without reconnecting.
            let flood_limit = self.config.borrow().flood_limit();
            match Client::from_config(cfg).await {
                Ok(mut c) => {
                    if c.identify().is_err() {
                        error!("Could not identify with server.");
                        return None;
                    }
                    match c.stream() {
                        Ok(cs) => {
                            // Re-read the nick actually in use; the server may have
                            // handed us an alternative if ours was taken.
                            self.our_nick = c.current_nickname().to_string();
                            // Publish this connection's sender for the
                            // notification backend; the previous one died with
                            // the previous connection.
                            // One pacer per connection, shared by command replies
                            // and notifications: they contend for the same
                            // socket, so the flood limit has to cover both.
                            let outbound = spawn_pacer(c.sender(), flood_limit.clone());
                            self.outbound = Some(outbound.clone());

                            // Presence starts optimistic and is corrected by the
                            // first ISON reply; assuming offline would hold every
                            // notification until a probe answered.
                            if let Ok(mut slot) = self.sender_slot.lock() {
                                *slot = crate::notify::IrcLink::connected(outbound);
                            }
                            self.client = Rc::new(RefCell::new(Some(c)));
                            info!("Connected to IRC server as {}.", self.our_nick);
                            Some(cs)
                        }
                        Err(e) => {
                            error!("Could not get client stream: {e:?}");
                            None
                        }
                    }
                }
                Err(e) => {
                    error!("Could not connect to IRC server. {e:?}");
                    None
                }
            }
        }
    }

    /// The two bounds on the identity check, tested against a clock they are
    /// handed rather than one they read -- a ten-minute cooldown is not
    /// something to verify by waiting.
    #[cfg(test)]
    mod whois_limits_test {
        use super::*;

        #[test]
        fn the_budget_allows_a_bounded_number_of_lookups_per_minute() {
            let start = Instant::now();
            let mut budget = WhoisBudget::new(start);

            for i in 0..WHOIS_PER_MINUTE {
                assert!(budget.take(start), "lookup {i} is within the budget");
            }
            assert!(!budget.take(start), "the budget must run out");
        }

        #[test]
        fn the_budget_refills_on_the_next_minute() {
            let start = Instant::now();
            let mut budget = WhoisBudget::new(start);
            for _ in 0..WHOIS_PER_MINUTE {
                budget.take(start);
            }
            assert!(!budget.take(start));

            // Just short of a minute is still the same window.
            assert!(!budget.take(start + Duration::from_secs(59)));
            assert!(budget.take(start + Duration::from_secs(60)));
        }

        #[test]
        fn a_nick_is_left_alone_after_repeated_failures() {
            let now = Instant::now();
            let mut cooldowns = WhoisCooldowns::default();

            for i in 1..WHOIS_FAILURE_LIMIT {
                assert!(
                    !cooldowns.note_failure("owner", now),
                    "failure {i} is below the limit, so nothing is said"
                );
                assert!(!cooldowns.is_cooling("owner", now));
            }

            assert!(
                cooldowns.note_failure("owner", now),
                "the last failure starts the cooldown, and says so once"
            );
            assert!(cooldowns.is_cooling("owner", now));
        }

        /// The message is sent when the cooldown begins and never again, or the
        /// bot answers every attempt and is an amplifier once more.
        #[test]
        fn the_cooldown_announces_itself_exactly_once() {
            let now = Instant::now();
            let mut cooldowns = WhoisCooldowns::default();
            let mut announced = 0;

            for _ in 0..(WHOIS_FAILURE_LIMIT as u32 * 3) {
                if cooldowns.note_failure("owner", now) {
                    announced += 1;
                }
            }
            assert_eq!(announced, 3, "one per completed run of failures, not one per failure");
        }

        #[test]
        fn a_check_that_passes_clears_the_failures() {
            let now = Instant::now();
            let mut cooldowns = WhoisCooldowns::default();

            cooldowns.note_failure("owner", now);
            cooldowns.note_success("owner");

            // Back to zero: the next failure must not be the one that trips it.
            for _ in 1..WHOIS_FAILURE_LIMIT {
                assert!(!cooldowns.note_failure("owner", now));
            }
            assert!(cooldowns.note_failure("owner", now));
        }

        #[test]
        fn the_cooldown_expires_and_is_pruned() {
            let now = Instant::now();
            let mut cooldowns = WhoisCooldowns::default();
            for _ in 0..WHOIS_FAILURE_LIMIT {
                cooldowns.note_failure("owner", now);
            }
            assert!(cooldowns.is_cooling("owner", now));

            let later = now + WHOIS_COOLDOWN + Duration::from_secs(1);
            assert!(!cooldowns.is_cooling("owner", later), "the cooldown must end");

            cooldowns.prune(later);
            assert!(cooldowns.0.is_empty(), "an expired entry must not be kept forever");
        }

        /// A nick part-way to a cooldown is not pruned, or the counter resets
        /// every sweep and the limit is never reached.
        #[test]
        fn pruning_keeps_a_nick_that_is_part_way_there() {
            let now = Instant::now();
            let mut cooldowns = WhoisCooldowns::default();
            cooldowns.note_failure("owner", now);

            cooldowns.prune(now);
            assert_eq!(cooldowns.0.len(), 1);
        }

        #[test]
        fn cooldowns_are_case_insensitive_like_every_other_nick_comparison() {
            let now = Instant::now();
            let mut cooldowns = WhoisCooldowns::default();

            for _ in 0..WHOIS_FAILURE_LIMIT {
                cooldowns.note_failure("Owner", now);
            }
            assert!(cooldowns.is_cooling("owner", now));
            assert!(cooldowns.is_cooling("OWNER", now));

            cooldowns.note_success("oWnEr");
            assert!(!cooldowns.is_cooling("Owner", now));
        }
    }
}

#[cfg(test)]
pub mod test {
    #[tokio::test]
    pub async fn test_regex() {
        let re: regex::Regex = regex::Regex::new(r".*Name:'(?P<name>.*)' uploaded by.*https://tracker.example.org/torrent/(?P<id>\d+)").unwrap();
        let caps = re.captures("New Torrent Announcement: <TV :: BoxSets>  Name:'Secrets of Sulphur Springs S01 1080p AMZN WEB-DL DDP5 1 H 264-TVSmash' uploaded by 'Anonymous' freeleech -  https://tracker.example.org/torrent/241240312").unwrap();
        assert_eq!(&caps["name"], "Secrets of Sulphur Springs S01 1080p AMZN WEB-DL DDP5 1 H 264-TVSmash");
        assert_eq!(&caps["id"], "241240312");
    }

    /// `cmd:addtorrent` carries an announce line as its parameter, so it matches
    /// the announce regex as well as the command regex. msg_process must
    /// therefore test for a command first; testing the announce regex first made
    /// the command unreachable, and the only symptom was the announcement
    /// handler rejecting it as coming from an unconfigured source.
    #[test]
    fn an_addtorrent_command_also_matches_the_announce_regex() {
        let announce = regex::Regex::new(
            r".*Name:'(?P<name>.*)' uploaded by.*https://tracker.example.org/torrent/(?P<id>\d+)",
        )
        .unwrap();
        // The real pattern, not a copy: this test used to carry its own literal,
        // so it went on passing while the production matcher rejected the
        // `command:` spelling and quietly handled it as an announcement.
        let command =
            regex::Regex::new(crate::command_processor::commands::COMMAND_PATTERN).unwrap();

        let msg = "cmd:addtorrent params:(New Torrent Announcement: Name:'Some Release 1080p' \
                   uploaded by 'Anon' - https://tracker.example.org/torrent/12345)";

        assert!(command.is_match(msg), "should be recognised as a command");
        assert!(
            announce.is_match(msg),
            "and it unavoidably matches the announce regex too -- which is why \
             msg_process has to check is_command() first"
        );
    }

    #[test]
    fn nickserv_status_regex_does_not_panic_on_odd_input() {
        let re = regex::Regex::new(r"STATUS (?P<nick>\S+) (?P<status>\d)").unwrap();
        // Messages that mention STATUS but do not match must simply not capture,
        // rather than panicking as the previous `.unwrap()` chain did.
        assert!(re.captures("STATUS").is_none());
        assert!(re.captures("your STATUS is unknown").is_none());
        let caps = re.captures("STATUS someone 3").unwrap();
        assert_eq!(&caps["nick"], "someone");
        assert_eq!(&caps["status"], "3");
    }

    /// `stop!` cancels every reply outstanding, not just the newest request --
    /// with two listings still draining, "stop" plainly means both -- while
    /// notifications queued behind them survive. Being told to stop listing
    /// torrents is not a request to throw away a download-finished alert.
    #[test]
    fn stop_discards_queued_replies_but_keeps_notifications() {
        use crate::irc_processor::irc::Outbound;

        let (queue, mut rx) = Outbound::for_test();

        // Two overlapping listings, with an alert queued between them.
        queue.send("owner", "listing A line 1");
        queue.send("owner", "listing A line 2");
        queue.send_uninterruptible("owner", "Finished: Some Release");
        queue.send("owner", "listing B line 1");

        assert_eq!(queue.cancel(), 3, "the three replies, not the alert");
        let epoch = queue.current_epoch();

        // Anything queued after the stop -- including the stop's own reply --
        // carries the new epoch and survives.
        queue.send("owner", "Stopped; 3 queued messages discarded.");

        let mut delivered = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if !Outbound::discards(&msg, epoch) {
                delivered.push(msg);
            }
        }

        let texts: Vec<&str> = delivered.iter().map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec!["Finished: Some Release", "Stopped; 3 queued messages discarded."],
            "only the alert and the post-stop reply should survive"
        );
    }

    /// A second `stop!` with nothing queued must not claim to have dropped
    /// anything, and must not disturb what is already in flight.
    #[test]
    fn a_stop_with_an_empty_queue_reports_nothing() {
        use crate::irc_processor::irc::Outbound;

        let (queue, _rx) = Outbound::for_test();
        assert_eq!(queue.cancel(), 0);

        queue.send("owner", "one");
        assert_eq!(queue.cancel(), 1);
        assert_eq!(queue.cancel(), 0, "the same messages must not be counted twice");
    }

    /// The numerics the identity check depends on must actually parse into the
    /// shapes `msg_process` matches on. 330 is not in irc-proto's Response enum,
    /// so it has to arrive as `Raw` -- if a future version adds it, the match
    /// silently stops firing and every command would be refused.
    #[test]
    fn the_whois_numerics_parse_into_the_expected_shapes() {
        use irc::proto::{Command, Message, Response};

        let account: Message = ":server 330 bot alice AliceAcct :is logged in as".parse().unwrap();
        match &account.command {
            Command::Raw(code, args) => {
                assert_eq!(code, "330");
                assert_eq!(args[1], "alice", "nick is the second parameter");
                assert_eq!(args[2], "AliceAcct", "account is the third");
            }
            other => panic!("330 should arrive raw, got {other:?}"),
        }

        let end: Message = ":server 318 bot alice :End of /WHOIS list.".parse().unwrap();
        match &end.command {
            Command::Response(Response::RPL_ENDOFWHOIS, args) => assert_eq!(args[1], "alice"),
            other => panic!("318 should be RPL_ENDOFWHOIS, got {other:?}"),
        }

        let ison: Message = ":server 303 bot :alice bob".parse().unwrap();
        match &ison.command {
            Command::Response(Response::RPL_ISON, args) => {
                assert_eq!(args.last().unwrap(), "alice bob", "the list is the trailing param");
            }
            other => panic!("303 should be RPL_ISON, got {other:?}"),
        }
    }

    /// An empty ISON reply means nobody asked about is online -- the shape a
    /// server sends when the owner has disconnected.
    #[test]
    fn an_empty_ison_reply_names_nobody() {
        use irc::proto::{Command, Message, Response};

        let ison: Message = ":server 303 bot :".parse().unwrap();
        let Command::Response(Response::RPL_ISON, args) = &ison.command else {
            panic!("expected RPL_ISON");
        };
        let list = args.last().map(String::as_str).unwrap_or("");
        assert!(!list.split_whitespace().any(|n| n.eq_ignore_ascii_case("alice")));
    }

    /// A torrent name is arbitrary bytes from a `.torrent`, and the bot echoes it
    /// back over IRC. Without this, a name carrying CRLF would end the PRIVMSG
    /// early and the remainder would reach the server as its own command.
    #[test]
    fn a_reply_cannot_carry_a_line_break_into_the_protocol() {
        use crate::irc_processor::irc::sanitize_for_irc;

        let out = sanitize_for_irc("Some Release\r\nJOIN #somewhere");
        assert!(!out.contains('\r') && !out.contains('\n'), "{out:?}");
        assert_eq!(out, "Some Release  JOIN #somewhere");

        assert_eq!(sanitize_for_irc("a\0b"), "a b");
        // Ordinary text is untouched.
        assert_eq!(sanitize_for_irc("2 torrents: Alpha; Beta"), "2 torrents: Alpha; Beta");
    }
}
