pub mod config {
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::Error;
    use directories::BaseDirs;
    use log::{debug, error, info, warn};
    use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
    use regex::Regex;
    use serde::{de, ser};
    use serde_derive::{Deserialize, Serialize};
    use tokio::fs;

    use crate::platforms::url_template::UrlTemplate;
    use crate::{IRC_CONFIG_FILE, OPTIONS_CONFIG_FILE};

    /// How long to wait for a burst of filesystem events to settle before
    /// reloading. A single editor save emits four or more events, and an
    /// atomically replaced file is briefly absent or partially written, so
    /// coalescing serves both purposes.
    const RELOAD_DEBOUNCE: Duration = Duration::from_millis(250);

    /// The parsed options plus the regexes compiled from them.
    ///
    /// The accessors used to call `Regex::new` on every use, which meant
    /// recompiling every download pattern for every announcement, and an invalid
    /// pattern panicked at message time. Compiling here means a bad pattern is
    /// rejected when the file is read -- which is exactly the validation a
    /// reload needs before it swaps anything in.
    pub struct LoadedOptions {
        data: OptionData,
        announce_regex: Regex,
        dl_regexes: Vec<Regex>,
        reject_regexes: Vec<Regex>,
    }

    impl LoadedOptions {
        fn from_data(data: OptionData) -> Result<Self, Error> {
            let announce_regex = Regex::new(&data.regex_for_announce_match).map_err(|e| {
                Error::msg(format!("regex_for_announce_match is not a valid regex: {e}"))
            })?;

            let compile_all = |patterns: &Vec<String>, field: &str| -> Result<Vec<Regex>, Error> {
                patterns
                    .iter()
                    .map(|p| {
                        Regex::new(p)
                            .map_err(|e| Error::msg(format!("{field} contains an invalid regex: {e}")))
                    })
                    .collect()
            };

            let dl_regexes = compile_all(&data.regex_for_downloads_match, "regex_for_downloads_match")?;
            let reject_regexes = compile_all(
                &data.regex_for_downloads_reject_match,
                "regex_for_downloads_reject_match",
            )?;

            // After the regexes, so the three tests that assert on regex field
            // names keep getting the regex error rather than this one.
            validate_platform(&data.platform)?;

            Ok(Self { data, announce_regex, dl_regexes, reject_regexes })
        }
    }

    /// Reject a tracker section that cannot actually download anything.
    ///
    /// Runs at startup *and* on every reload, so blanking the template in a
    /// running bot is refused and the working config kept -- the same treatment
    /// a broken regex gets.
    fn validate_platform(p: &PlatformSection) -> Result<(), Error> {
        let o = &p.options;

        if o.download_url_template.trim().is_empty() {
            return Err(Error::msg(format!(
                "options.toml: [platform.{}] has no download_url_template.\n\
                 Set it to the URL your tracker serves .torrent files from, using \
                 {{id}}, {{name}}, {{file}} and {{key}} as placeholders, for example:\n\n    \
                 download_url_template = \"https://tracker.example.org/rss/download/{{id}}/{{key}}/{{file}}\"\n\n\
                 See docs/options.sample.toml. Nothing is downloaded until this is set.",
                p.label
            )));
        }

        let template = UrlTemplate::parse(&o.download_url_template)
            .map_err(|e| Error::msg(format!("options.toml: [platform.{}]: {e}", p.label)))?;

        if template.uses_key() && o.rss_key.trim().is_empty() {
            return Err(Error::msg(format!(
                "options.toml: [platform.{}] uses {{key}} in download_url_template \
                 but rss_key is empty",
                p.label
            )));
        }

        // An empty torrent_dir makes the output path relative, and `dir` then
        // equals `candidate.parent()` -- so `assert_within` passes and .torrent
        // files land in whatever the process working directory happens to be.
        if o.torrent_dir.trim().is_empty() {
            return Err(Error::msg(format!(
                "options.toml: [platform.{}] has an empty torrent_dir",
                p.label
            )));
        }

        Ok(())
    }

    /// Reject an irc.toml that names no network, and warn about one that names
    /// no channel.
    ///
    /// The server is fatal: there is nothing to connect to without it, and the
    /// shipped default is deliberately empty. An empty channel list is not --
    /// driving the bot entirely by private message is a legitimate way to run
    /// it -- but `Authorization::is_valid_channel` gates announcements on this
    /// list, so an empty one means every announcement is silently ignored, and
    /// that deserves saying out loud.
    fn validate_irc(cfg: &irc::client::data::config::Config) -> Result<(), Error> {
        if cfg.server.as_deref().unwrap_or("").trim().is_empty() {
            return Err(Error::msg(
                "irc.toml has no server. Set it to your announce network, for example:\n\n    \
                 server = \"irc.example.net\"\n    port = 6697\n    use_tls = true\n\n\
                 irc2torrent ships no network of its own.",
            ));
        }
        if cfg.channels.is_empty() {
            warn!(
                "irc.toml lists no channels: announcements cannot be seen, so nothing will be \
                 added automatically. Commands sent directly to the bot still work."
            );
        }
        Ok(())
    }

    /// The outbound rate limit, shared live with the pacer that enforces it.
    ///
    /// Separated from the rest of irc.toml because it is the one setting there
    /// that does *not* need a reconnect: server, port and nickname are handed to
    /// the client when the connection is built, but this only ever governs how
    /// fast the pacer drains its queue. Tuning it after a flood kick should not
    /// require restarting the bot.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FloodLimit {
        pub window: Duration,
        pub max_in_burst: usize,
    }

    pub type SharedFloodLimit = Arc<Mutex<FloodLimit>>;

    /// A live view of `[notifications]`, for the parts of the bot that run off
    /// the main task.
    ///
    /// The config is *already* shared: `Config::option_data` is an
    /// `Arc<Mutex<LoadedOptions>>` that the watcher thread writes through, and
    /// `SharedFloodLimit` above is the same idea -- reading through it per
    /// message is exactly why the IRC burst settings apply on save. What made
    /// notifications restart-only was not the container but the consumers: the
    /// dispatcher and the poller each took a *clone by value* at startup and
    /// never looked again.
    ///
    /// (An `ArcSwap` would be a nicer container -- lock-free reads and no
    /// poisoning to handle -- but it would not have changed that, and the write
    /// path would need a compare-and-swap loop to keep watchlist edits atomic,
    /// which the mutex gives for free. The lock is taken a few times a minute by
    /// one writer and a handful of readers.)
    #[derive(Clone)]
    pub struct SharedNotificationOptions(Arc<Mutex<LoadedOptions>>);

    impl SharedNotificationOptions {
        /// A fresh copy of the section, or `None` if the lock is poisoned.
        ///
        /// A copy rather than a guard because every caller holds the value
        /// across an `await`, and a `MutexGuard` is not `Send`. The section is
        /// small and this runs once per dispatcher wake-up, not per message.
        pub fn get(&self) -> Option<NotificationOptions> {
            match self.0.lock() {
                Ok(guard) => Some(guard.data.notifications.clone()),
                Err(e) => {
                    // Keep running on the last known config: a poisoned lock is
                    // not worth silencing notifications over.
                    error!("Config lock poisoned, keeping the running notification config: {e}");
                    None
                }
            }
        }
    }

    impl FloodLimit {
        /// The effective limit for an irc.toml, applying the same defaults
        /// `get_irc_config` substitutes for a file that predates these settings.
        pub fn from_irc(cfg: &irc::client::data::config::Config) -> Self {
            Self {
                window: Duration::from_secs(cfg.burst_window_length.unwrap_or(8) as u64),
                // Zero would stall the queue outright.
                max_in_burst: (cfg.max_messages_in_burst.unwrap_or(5) as usize).max(1),
            }
        }
    }

    pub struct Config {
        option_data: Arc<Mutex<LoadedOptions>>,
        /// Read by the pacer on every message, so an edit to irc.toml takes
        /// effect on the next one.
        flood_limit: SharedFloodLimit,
        /// Filled in after construction: the notifier needs the config to exist
        /// first, and the watcher thread that reports a rejected reload starts
        /// here. Shared so a later `set_notifier` reaches that thread.
        notifier: Arc<Mutex<crate::notify::Notifier>>,
        irc_data: irc::client::data::config::Config,
        /// Held only to keep the watch alive: dropping the watcher stops it.
        _watcher: Option<RecommendedWatcher>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct OptionData {
        platform: PlatformSection,
        clients: Vec<TorrentClientOption>,
        command_options: CommandOptions,
        regex_for_downloads_match: Vec<String>,
        regex_for_downloads_reject_match: Vec<String>,
        regex_for_announce_match: String,
        /// Absent in configs written before notifications existed, so it must
        /// default rather than fail the whole parse and take the bot down on
        /// upgrade.
        #[serde(default)]
        notifications: NotificationOptions,
        /// Two-way transports, which carry commands *and* notifications.
        ///
        /// Separate from `[notifications]` on purpose: email and ntfy are
        /// one-way sinks and can never receive a command, while these own
        /// credentials, an owner and both roles. Tables, so they must stay last
        /// -- a bare key cannot follow a table header in TOML.
        #[serde(default)]
        telegram: Option<TelegramOptions>,
        #[serde(default)]
        slack: Option<SlackOptions>,
    }

    fn default_true_role() -> bool {
        true
    }

    /// Telegram, as both a command source and a notification target.
    ///
    /// The Bot API is plain HTTPS long polling, so this needs no inbound port
    /// and no new dependency -- reqwest, serde_json and tokio are already here.
    #[derive(Clone, PartialEq, Serialize, Deserialize)]
    pub struct TelegramOptions {
        /// From @BotFather. A credential: never logged.
        pub token: String,
        /// The only user whose messages are obeyed, and where notifications go.
        ///
        /// A platform-issued ID, so unlike an IRC nick it cannot be taken by
        /// somebody else; there is no NickServ equivalent to check.
        pub owner_id: i64,
        #[serde(default = "default_true_role")]
        pub commands: bool,
        #[serde(default = "default_true_role")]
        pub notifications: bool,
        /// Declared last: serialises as a table.
        #[serde(default)]
        pub events: EventFilter,
    }

    // Hand-written so the token cannot reach a log through a stray `{:?}`.
    impl std::fmt::Debug for TelegramOptions {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TelegramOptions")
                .field("token", &"<redacted>")
                .field("owner_id", &self.owner_id)
                .field("commands", &self.commands)
                .field("notifications", &self.notifications)
                .finish()
        }
    }

    /// Slack, posting to one channel in a workspace you control.
    ///
    /// Uses **Socket Mode**, which exists for apps with no public URL: the bot
    /// opens an outbound WebSocket rather than being called back. That keeps the
    /// property that made Telegram attractive -- no inbound port, nothing to
    /// forward on the router. Slack's other option, the Events API, would need a
    /// public HTTPS endpoint.
    ///
    /// Two tokens, because Slack separates them: `app_token` opens the socket,
    /// `bot_token` posts messages.
    #[derive(Clone, PartialEq, Serialize, Deserialize)]
    pub struct SlackOptions {
        /// `xapp-…`, from Basic Information → App-Level Tokens, scope
        /// `connections:write`. Opens the Socket Mode connection.
        pub app_token: String,
        /// `xoxb-…`, from OAuth & Permissions, scope `chat:write`. Posts.
        pub bot_token: String,
        /// Where the bot reads and writes, e.g. `C01234567`.
        ///
        /// Omit it and the bot talks to `owner_id` directly instead. That is
        /// the private option: a channel shows every reply to everyone in it,
        /// and a torrent listing is not usually meant for an audience.
        #[serde(default)]
        pub channel_id: Option<String>,
        /// The only user whose messages are obeyed, e.g. `U01234567`.
        pub owner_id: String,
        #[serde(default = "default_true_role")]
        pub commands: bool,
        #[serde(default = "default_true_role")]
        pub notifications: bool,
        #[serde(default)]
        pub events: EventFilter,
    }

    impl std::fmt::Debug for SlackOptions {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SlackOptions")
                .field("app_token", &"<redacted>")
                .field("bot_token", &"<redacted>")
                .field("channel_id", &self.channel_id)
                .field("owner_id", &self.owner_id)
                .field("commands", &self.commands)
                .field("notifications", &self.notifications)
                .finish()
        }
    }

    /// The client a freshly written `options.toml` should point at.
    ///
    /// Two things this fixes. The default used to list rTorrent *and* Flood,
    /// but only the first entry is ever used -- so every new install got a
    /// warning about an ignored second client for no reason. And the
    /// qBittorrent image would write an rTorrent config, so a container that
    /// ships no rTorrent at all spent its first run failing to reach one.
    ///
    /// The image says which it is; unset means rTorrent, as before.
    fn default_client() -> TorrentClientOption {
        match std::env::var("IRC2TORRENT_DEFAULT_CLIENT").unwrap_or_default().as_str() {
            "qBittorrent" | "qbittorrent" => {
                TorrentClientOption::QBittorrent(QBittorrentOptions::default())
            }
            "Flood" | "flood" => TorrentClientOption::Flood(FloodOptions::default()),
            _ => TorrentClientOption::rTorrent(rTorrentOptions::default()),
        }
    }

    impl Default for OptionData {
        fn default() -> Self {
            Self {
                platform: PlatformSection::default(),
                clients: vec![default_client()],
                command_options: CommandOptions::default(),
                regex_for_downloads_match: vec!["Some Regex to match.*1080p.*".to_string(), "Another Release.*S02.*1080p.*WEB.*".to_string()],
                regex_for_downloads_reject_match: vec![".*NORDIC.*".to_string(), ".*GERMAN.*".to_string()],
                // An example of the shape, not a working pattern for any
                // particular network: every tracker announces differently, so
                // this is the field a user always has to write themselves.
                regex_for_announce_match: r".*Name:'(?P<name>.*)' uploaded by.*https://tracker\.example\.org/torrent/(?P<id>\d+)".to_string(),
                notifications: NotificationOptions::default(),
                telegram: None,
                slack: None,
            }
        }
    }

    /// Which events to report and where.
    ///
    /// Every backend is optional and each one switches itself on only when its
    /// own config is present and usable, so an untouched section sends nothing
    /// and costs nothing.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    pub struct NotificationOptions {
        // -- events -------------------------------------------------------
        /// Add failures, crash loops, a rejected config, IRC down for a while.
        pub on_failure: bool,
        /// Every successful add. Off by default: an announce channel is busy,
        /// and this is the one people enable and then immediately regret.
        pub on_torrent_added: bool,
        /// A torrent reaching 100%.
        pub on_download_finished: bool,
        /// Free space on `disk_path` falling below `disk_warn_percent`.
        pub on_disk_low: bool,
        /// One roll-up per day of everything above.
        pub daily_summary: bool,
        /// The bot coming up, and IRC coming back after an outage that was
        /// reported. On by default: it is the answer to "did my restart work?",
        /// it happens once per start, and a crash loop collapses into a count
        /// like every other repeat.
        pub on_start: bool,

        // -- shaping ------------------------------------------------------
        /// Events are buffered this long and sent as one message. The single
        /// setting that decides whether this feature is usable: without it a
        /// busy channel is one notification per release.
        pub digest_seconds: u64,
        /// Ceiling on messages per hour per backend. What is dropped is counted
        /// and reported in the next message rather than vanishing.
        pub max_per_hour: u32,
        /// How often to ask the client what has finished, in seconds.
        pub poll_seconds: u64,
        /// Filesystem to watch, and the threshold to warn at.
        pub disk_path: String,
        pub disk_warn_percent: f64,

        // -- backends -----------------------------------------------------
        //
        // Tables, so they must stay last: TOML cannot have a bare key after a
        // table header, and these are serialised in declaration order when the
        // default config is written out.
        //
        // A backend is on when its table is present *and* usable. Each may
        // override any of the event switches above in its own `[..events]`
        // table -- so ntfy can carry only failures while email takes everything.
        pub email: Option<EmailOptions>,
        pub ntfy: Option<NtfyOptions>,
        /// Private message the owner. Needs no settings of its own: the bot is
        /// already connected and already knows the nick from
        /// `command_options.security_mode`, so the empty table is enough.
        pub irc: Option<IrcNotifyOptions>,
    }

    /// Per-backend event overrides. `None` means "use the global switch".
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    pub struct EventFilter {
        pub on_failure: Option<bool>,
        pub on_torrent_added: Option<bool>,
        pub on_download_finished: Option<bool>,
        pub on_disk_low: Option<bool>,
        pub daily_summary: Option<bool>,
        pub on_start: Option<bool>,
    }

    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct IrcNotifyOptions {
        #[serde(default)]
        pub events: EventFilter,
    }

    impl Default for NotificationOptions {
        fn default() -> Self {
            Self {
                on_failure: true,
                on_torrent_added: false,
                on_download_finished: true,
                on_disk_low: true,
                daily_summary: false,
                on_start: true,
                digest_seconds: 300,
                max_per_hour: 20,
                poll_seconds: 120,
                disk_path: "/downloads".to_string(),
                disk_warn_percent: 10.0,
                email: None,
                ntfy: None,
                irc: None,
            }
        }
    }

    /// SMTP settings, with as much as possible inferred.
    ///
    /// `address` and `password` are usually the whole configuration: the server
    /// is looked up from the address's domain, the port and TLS mode follow from
    /// convention, and both `from` and `to` default to `address` -- you are
    /// nearly always mailing yourself.
    #[derive(Clone, PartialEq, Serialize, Deserialize)]
    pub struct EmailOptions {
        pub address: String,
        /// Empty is allowed here so the secret can come from
        /// `IRC2TORRENT_SMTP_PASSWORD` or `password_file` instead of sitting in
        /// a config file that gets bind-mounted around.
        #[serde(default)]
        pub password: String,
        #[serde(default)]
        pub password_file: Option<String>,
        #[serde(default)]
        pub host: Option<String>,
        #[serde(default)]
        pub port: Option<u16>,
        #[serde(default)]
        pub from: Option<String>,
        #[serde(default)]
        pub to: Option<String>,
        /// Declared last: it serialises as a table, and TOML forbids a bare key
        /// after a table header.
        #[serde(default)]
        pub events: EventFilter,
    }

    // Hand-written so the password cannot reach a log through a stray `{:?}`,
    // the same way FloodOptions does it.
    impl std::fmt::Debug for EmailOptions {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("EmailOptions")
                .field("address", &self.address)
                .field("password", &"<redacted>")
                .field("password_file", &self.password_file)
                .field("host", &self.host)
                .field("port", &self.port)
                .field("from", &self.from)
                .field("to", &self.to)
                .finish()
        }
    }

    /// ntfy: a topic name is the entire setup -- no account, no API key.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct NtfyOptions {
        /// Topic on ntfy.sh, or a full URL for a self-hosted instance.
        ///
        /// Anyone who knows the topic can read it on the public server, so treat
        /// it as a secret and pick something unguessable.
        pub topic: String,
        /// Bearer token, for a server that requires auth.
        #[serde(default)]
        pub token: Option<String>,
        /// Last, for the same TOML reason as EmailOptions::events.
        #[serde(default)]
        pub events: EventFilter,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct CommandOptions {
        security_mode: SecurityMode,
        commands_enabled: bool,
        /// Require the sender to be identified to network services before any
        /// command runs, in `IrcUserName` mode.
        ///
        /// Without it the only credential is a nickname, which is not one: on a
        /// network that does not enforce registration, anyone can take yours the
        /// moment you disconnect and then command the bot. With it, every command
        /// costs one WHOIS and is refused unless services confirm the sender is
        /// logged in to the matching account.
        ///
        /// Defaults on. It is a security control, and when a network cannot
        /// answer the check the failure is loud -- commands are refused with a
        /// message naming this setting -- rather than silently permissive.
        #[serde(default = "default_true")]
        require_identified: bool,
        /// How many lines a listing reply may span.
        ///
        /// Each line is its own PRIVMSG, and servers kill a client that sends
        /// too many too quickly. The irc crate lets 15 out as an instant burst
        /// before it starts throttling, and a real network's flood limit is
        /// tripped well before that -- so this is deliberately below it.
        ///
        /// Raise it with `max_messages_in_burst` in irc.toml, not on its own.
        #[serde(default = "default_max_reply_lines")]
        max_reply_lines: usize,
    }

    fn default_true() -> bool {
        true
    }

    fn default_max_reply_lines() -> usize {
        12
    }

    impl Default for CommandOptions {
        fn default() -> Self {
            Self {
                security_mode: SecurityMode::IrcUserName("irc2torrent".to_string()),
                commands_enabled: false,
                require_identified: true,
                max_reply_lines: default_max_reply_lines(),
            }
        }
    }

    #[derive(Clone, PartialEq, Serialize, Deserialize)]
    pub enum SecurityMode {
        Password(String),
        IrcUserName(String),
    }

    // The Password variant holds the bot's command password verbatim.
    impl std::fmt::Debug for SecurityMode {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                SecurityMode::Password(_) => f.write_str("Password(<redacted>)"),
                SecurityMode::IrcUserName(u) => write!(f, "IrcUserName({u})"),
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub enum TorrentClientOption {
        rTorrent(rTorrentOptions),
        Flood(FloodOptions),
        /// Renamed for serde so the TOML table reads `[clients.qBittorrent]`,
        /// matching how the project spells it and how `rTorrent` above already
        /// looks, without adding another non_camel_case_types warning.
        #[serde(rename = "qBittorrent")]
        QBittorrent(QBittorrentOptions),
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct rTorrentOptions {
        pub xmlrpc_url: String,
    }

    impl Default for rTorrentOptions {
        fn default() -> Self {
            Self {
                // One slash after `unix:`, not two. A `//` opens an authority,
                // so `config` would be parsed as a hostname and the socket
                // path would come out as `/.local/share/rtorrent/rtorrent.sock`.
                // See the_unix_socket_url_must_not_use_a_double_slash below.
                xmlrpc_url: "unix:/config/.local/share/rtorrent/rtorrent.sock".to_string(),
            }
        }
    }

    /// qBittorrent over its WebUI API.
    ///
    /// `username`/`password` may both be empty, which is the normal case when
    /// qBittorrent bypasses authentication for localhost -- what the bundled
    /// image ships. The client never logs in unless the server asks it to.
    #[derive(Clone, PartialEq, Serialize, Deserialize)]
    pub struct QBittorrentOptions {
        /// Where the WebUI listens, e.g. `http://127.0.0.1:8080`. Prefer the
        /// literal address over `localhost`: which family that resolves to
        /// inside a container is not worth gambling on.
        pub(crate) url: String,
        #[serde(default)]
        pub(crate) username: String,
        #[serde(default)]
        pub(crate) password: String,
        /// Empty means whatever qBittorrent is configured to use.
        #[serde(default)]
        pub(crate) save_path: String,
        /// Empty means no category.
        #[serde(default)]
        pub(crate) category: String,
    }

    // Hand-written Debug so the password cannot reach a log via a stray `{:?}`,
    // the same as FloodOptions below.
    impl std::fmt::Debug for QBittorrentOptions {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("QBittorrentOptions")
                .field("url", &self.url)
                .field("username", &self.username)
                .field("password", &"<redacted>")
                .field("save_path", &self.save_path)
                .field("category", &self.category)
                .finish()
        }
    }

    impl Default for QBittorrentOptions {
        fn default() -> Self {
            Self {
                url: "http://127.0.0.1:8080".to_string(),
                username: String::new(),
                password: String::new(),
                save_path: String::new(),
                category: String::new(),
            }
        }
    }

    #[derive(Clone, PartialEq, Serialize, Deserialize)]
    pub struct FloodOptions {
        pub(crate) url: String,
        pub(crate) username: String,
        pub(crate) password: String,
        pub(crate) destination: String,
    }

    // Hand-written Debug so the password cannot reach a log via a stray `{:?}`.
    impl std::fmt::Debug for FloodOptions {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("FloodOptions")
                .field("url", &self.url)
                .field("username", &self.username)
                .field("password", &"<redacted>")
                .field("destination", &self.destination)
                .finish()
        }
    }

    impl Default for FloodOptions {
        fn default() -> Self {
            Self {
                url: "http://localhost:3000".to_string(),
                username: "admin".to_string(),
                password: "password".to_string(),
                destination: "/downloads".to_string(),
            }
        }
    }

    /// The tracker, written as `[platform.<Label>]`.
    ///
    /// The label is the user's to choose and is what turns up in the log. This
    /// used to be a serde enum with a single variant, which meant the only key
    /// that parsed was that one variant's name -- while the README told people
    /// to invent their own and get `unknown variant` on startup.
    ///
    /// Modelled as a one-entry map rather than a `HashMap` so the arity is
    /// enforced by the parser: "no platform table" and "two platform tables"
    /// become precise errors instead of a missing-field message or a coin flip.
    /// Serialisation is deterministic for the same reason it matters here --
    /// `mutate_options` rewrites this file on every watchlist edit, and a lossy
    /// round trip would destroy real config.
    #[derive(Debug, Clone, PartialEq)]
    pub struct PlatformSection {
        pub(crate) label: String,
        pub(crate) options: PlatformOptions,
    }

    impl<'de> de::Deserialize<'de> for PlatformSection {
        fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            type Table = std::collections::BTreeMap<String, PlatformOptions>;
            let map = <Table as de::Deserialize>::deserialize(d)?;
            let mut it = map.into_iter();
            match (it.next(), it.next()) {
                (Some((label, options)), None) => Ok(Self { label, options }),
                (None, _) => Err(de::Error::custom(
                    "no [platform.<label>] table; add one, e.g. [platform.YourTracker]",
                )),
                (Some((a, _)), Some((b, _))) => Err(de::Error::custom(format!(
                    "two platform tables ([platform.{a}] and [platform.{b}]); \
                     irc2torrent drives one tracker, so keep one"
                ))),
            }
        }
    }

    impl ser::Serialize for PlatformSection {
        fn serialize<S: ser::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            use ser::SerializeMap;
            let mut m = s.serialize_map(Some(1))?;
            m.serialize_entry(&self.label, &self.options)?;
            m.end()
        }
    }

    #[derive(Clone, PartialEq, Serialize, Deserialize)]
    pub struct PlatformOptions {
        /// Where a .torrent is fetched from, with `{id}`, `{name}`, `{file}` and
        /// `{key}` filled in per download. There is deliberately no usable
        /// default: this is the one field that cannot be guessed, and guessing
        /// wrong means quietly fetching from somebody else's tracker.
        pub(crate) download_url_template: String,
        /// The per-user download key, where the tracker has one. A secret: never
        /// logged, never put in an error. Optional, and required only when the
        /// template mentions `{key}`.
        #[serde(default)]
        pub(crate) rss_key: String,
        pub(crate) torrent_dir: String,
    }

    // The rss_key authenticates downloads from the tracker; keep it out of logs.
    impl std::fmt::Debug for PlatformOptions {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("PlatformOptions")
                .field("download_url_template", &self.download_url_template)
                .field("rss_key", &"<redacted>")
                .field("torrent_dir", &self.torrent_dir)
                .finish()
        }
    }

    impl Default for PlatformOptions {
        fn default() -> Self {
            Self {
                // Empty on purpose -- see the doc comment above, and the
                // validation in LoadedOptions::from_data that turns this into a
                // startup error naming the field.
                download_url_template: String::new(),
                rss_key: String::new(),
                torrent_dir: "/downloads/.torrents".to_string(),
            }
        }
    }

    impl Default for PlatformSection {
        fn default() -> Self {
            Self {
                label: "YourTracker".to_string(),
                options: PlatformOptions::default(),
            }
        }
    }

    impl Config {
        pub async fn new() -> Result<Config, Error> {
            return if let (Some(option_config), Some(irc_config)) = (
                Config::read_or_create_toml::<OptionData>(
                    OPTIONS_CONFIG_FILE.to_string(),
                    Some(&OptionData::default()),
                )
                    .await,
                Config::read_or_create_toml::<irc::client::data::config::Config>(
                    IRC_CONFIG_FILE.to_string(),
                    Some(&Self::get_irc_default_config()),
                )
                    .await,
            ) {
                // Both checks run before either is reported. On a first run the
                // two config files have just been generated and both need
                // editing -- being told about the tracker, fixing it, and only
                // then being told about the IRC server is a poor way to find
                // that out.
                let loaded = LoadedOptions::from_data(option_config);
                let irc_ok = validate_irc(&irc_config);
                let loaded = match (loaded, irc_ok) {
                    (Ok(loaded), Ok(())) => loaded,
                    (options_result, irc_result) => {
                        let mut problems: Vec<String> = Vec::new();
                        if let Err(e) = options_result {
                            problems.push(e.to_string());
                        }
                        if let Err(e) = irc_result {
                            problems.push(e.to_string());
                        }
                        return Err(Error::msg(problems.join("\n\n")));
                    }
                };
                let option_data = Arc::new(Mutex::new(loaded));
                // Starts null and is replaced by set_notifier once the backends
                // are up; the watcher thread reads through the same Arc, so a
                // reload rejected later still reports.
                let notifier = Arc::new(Mutex::new(crate::notify::Notifier::disabled()));
                // Shared with the pacer, which reads it per message -- so an
                // edit to the burst settings applies without a reconnect.
                let flood_limit = Arc::new(Mutex::new(FloodLimit::from_irc(&irc_config)));
                let watcher =
                    Self::spawn_options_watcher(
                        Arc::clone(&option_data),
                        Arc::clone(&notifier),
                        Arc::clone(&flood_limit),
                    );

                Ok(Self {
                    option_data,
                    flood_limit,
                    notifier,
                    irc_data: irc_config,
                    _watcher: watcher,
                })
            } else {
                Self::hint_at_pre_template_options().await;
                Err(Error::msg("Could not read or create options file"))
            };
        }

        /// Say what to do when an options.toml predates `download_url_template`.
        ///
        /// `read_file_to_toml` now logs the serde error, which already names the
        /// missing field. This adds the one line to paste, because the field is
        /// new, has no default by design, and every existing install hits this
        /// exactly once on upgrade.
        async fn hint_at_pre_template_options() {
            let Some(path) = Self::get_full_config_path(OPTIONS_CONFIG_FILE.to_string()) else {
                return;
            };
            let Ok(text) = fs::read_to_string(&path).await else { return };
            let Ok(value) = text.parse::<toml::Value>() else { return };
            let Some(platform) = value.get("platform").and_then(toml::Value::as_table) else {
                return;
            };

            for (label, table) in platform {
                let has_key = table.get("rss_key").is_some();
                let has_template = table.get("download_url_template").is_some();
                if has_key && !has_template {
                    error!(
                        "[platform.{label}] in {} is from before the tracker became \
                         configurable. Add the URL your tracker serves .torrent files from:\n\n    \
                         download_url_template = \"https://your.tracker/rss/download/{{id}}/{{key}}/{{file}}\"\n\n\
                         The placeholders are {{id}}, {{name}}, {{file}} and {{key}}. \
                         See docs/options.sample.toml.",
                        path.display()
                    );
                    return;
                }
            }
        }

        /// Watch the config directory and reload `options.toml` when it changes.
        ///
        /// Watches the *directory*, not the file. A watch on the file follows the
        /// inode, and editors, `sed -i` and most config tooling save atomically
        /// (write a temporary file, then `rename()` over the original), which
        /// unlinks the watched inode and silently kills the watch after the very
        /// first save. Watching the directory and filtering by name survives that.
        ///
        /// Returns None if the watch could not be established; the bot still runs,
        /// just without live reload.
        fn spawn_options_watcher(
            shared: Arc<Mutex<LoadedOptions>>,
            notifier: Arc<Mutex<crate::notify::Notifier>>,
            flood_limit: SharedFloodLimit,
        ) -> Option<RecommendedWatcher> {
            let options_path = Config::get_full_config_path(OPTIONS_CONFIG_FILE.to_string())?;
            let config_dir = options_path.parent()?.to_path_buf();
            let options_name = options_path.file_name()?.to_os_string();
            let irc_name = std::ffi::OsString::from(IRC_CONFIG_FILE);

            // The notify handler runs on its own thread and must stay cheap, so it
            // only signals. A dedicated thread owns the debounce and the reload.
            //
            // `true` means options.toml, `false` means irc.toml. The directory is
            // already being watched, so noticing the second file costs nothing --
            // and an edit to irc.toml was previously ignored without a word,
            // which is the failure mode this bot keeps being bitten by.
            let (tx, rx) = mpsc::channel::<bool>();

            let mut watcher = match notify::recommended_watcher(move |res: notify::Result<Event>| match res {
                Ok(event) => {
                    for path in &event.paths {
                        let name = path.file_name();
                        if name == Some(options_name.as_os_str()) {
                            // A closed channel just means Config was dropped.
                            let _ = tx.send(true);
                        } else if name == Some(irc_name.as_os_str()) {
                            let _ = tx.send(false);
                        }
                    }
                }
                Err(e) => error!("Config watch error: {e:?}"),
            }) {
                Ok(w) => w,
                Err(e) => {
                    error!("Could not create config watcher, live reload disabled: {e}");
                    return None;
                }
            };

            if let Err(e) = watcher.watch(&config_dir, RecursiveMode::NonRecursive) {
                error!(
                    "Could not watch {}, live reload disabled: {e}",
                    config_dir.display()
                );
                return None;
            }

            let reload_path = options_path.clone();
            let irc_path = config_dir.join(IRC_CONFIG_FILE);
            std::thread::spawn(move || {
                // What irc.toml held when it was last reported on, so a repeated
                // event that changed nothing stays quiet.
                let mut last_irc = std::fs::read_to_string(&irc_path).ok();

                // Exits when the sender is dropped, i.e. when the watcher goes away.
                while let Ok(first) = rx.recv() {
                    // Coalesce the burst: one save emits several events, and an
                    // atomic replace leaves the file briefly missing or partial.
                    // Both files are tracked across the window, since saving them
                    // together is a single burst.
                    let mut options_changed = first;
                    let mut irc_changed = !first;
                    while let Ok(which) = rx.recv_timeout(RELOAD_DEBOUNCE) {
                        options_changed |= which;
                        irc_changed |= !which;
                    }

                    if options_changed {
                        Self::reload_options(&reload_path, &shared, &notifier);
                    }
                    if irc_changed {
                        Self::report_irc_change(&irc_path, &mut last_irc, &flood_limit);
                    }
                }
            });

            info!(
                "Watching {} for changes to {} and {}",
                config_dir.display(),
                OPTIONS_CONFIG_FILE,
                IRC_CONFIG_FILE
            );
            Some(watcher)
        }

        /// Say what happened when irc.toml is edited.
        ///
        /// It is deliberately *not* reloaded: the file is consumed once, when the
        /// client is built, so server, port, nickname and the flood settings all
        /// take effect at connect time. Saying nothing would be the worse option
        /// though -- editing a config and having it quietly ignored is precisely
        /// the failure that produced the `command:` bug, the addtime bug and the
        /// flood kick. It is parsed here too, so a typo surfaces now rather than
        /// at the next start, which may be days away and unattended.
        ///
        /// `last_seen` holds the contents from the previous report. Without that
        /// guard this is very noisy: `notify` can fall back to a polling backend
        /// that re-reports the file on every scan, and measured against one, a
        /// single edit produced 23 identical warnings and the next 41.
        /// `reload_options` is already immune because it compares the parsed data
        /// and skips when nothing changed.
        fn report_irc_change(
            path: &Path,
            last_seen: &mut Option<String>,
            flood_limit: &SharedFloodLimit,
        ) {
            let contents = match std::fs::read_to_string(path) {
                Ok(c) => c,
                // Expected transiently while an editor replaces the file.
                Err(e) => {
                    debug!("Could not read {} after a change: {e}", path.display());
                    return;
                }
            };

            if last_seen.as_deref() == Some(contents.as_str()) {
                return;
            }
            *last_seen = Some(contents.clone());

            match toml::from_str::<irc::client::data::config::Config>(&contents) {
                Ok(parsed) => {
                    // The flood limit is applied straight away: it only governs
                    // how fast the pacer drains its queue, so unlike the server,
                    // port and nickname it does not need a new connection. It is
                    // also the setting most likely to be tuned in a hurry, right
                    // after a network has objected to the current one.
                    let updated = FloodLimit::from_irc(&parsed);
                    match flood_limit.lock() {
                        Ok(mut current) => {
                            if *current != updated {
                                info!(
                                    "Flood limit now {} message(s) per {}s; applied immediately.",
                                    updated.max_in_burst,
                                    updated.window.as_secs()
                                );
                                *current = updated;
                            }
                        }
                        Err(e) => error!("Flood limit lock poisoned: {e}"),
                    }

                    warn!(
                        "{} changed. Everything except the flood settings is read once at \
                         startup, so server, port and nickname take effect only after a restart.",
                        path.display()
                    );
                }
                Err(e) => error!(
                    "{} changed but is not valid: {e}. Fix it before restarting, or the bot \
                     will not come back up.",
                    path.display()
                ),
            }
        }

        /// Re-read, validate and swap in the options. Never panics, never leaves
        /// the running config in a worse state than it found it.
        fn reload_options(
            path: &Path,
            shared: &Arc<Mutex<LoadedOptions>>,
            notifier: &Arc<Mutex<crate::notify::Notifier>>,
        ) {
            // A rejected reload is otherwise only a log line, and a config file
            // is usually edited by someone who then walks away assuming it took.
            let reject = |reason: String| {
                if let Ok(n) = notifier.lock() {
                    n.send(crate::notify::Event::ConfigRejected(reason));
                }
            };

            let contents = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(e) => {
                    // Expected transiently while a file is being replaced.
                    debug!("Could not read {} during reload: {e}", path.display());
                    return;
                }
            };

            let parsed: OptionData = match toml::from_str(&contents) {
                Ok(d) => d,
                Err(e) => {
                    error!("{} is not valid, keeping the running config: {e}", path.display());
                    reject(format!("{} is not valid TOML: {e}", path.display()));
                    return;
                }
            };

            // Cheap because OptionData derives PartialEq. This suppresses the
            // reload triggered by our own writes in add_dl_regex/remove_dl_regex,
            // and any event that did not actually change anything.
            {
                let current = match shared.lock() {
                    Ok(g) => g,
                    Err(e) => {
                        error!("Config lock poisoned, skipping reload: {e}");
                        return;
                    }
                };
                if current.data == parsed {
                    debug!("{} changed but its contents are identical; ignoring", path.display());
                    return;
                }

                // Values consumed once at startup cannot take effect without
                // rebuilding the client and platform objects. Say so rather than
                // letting the change look applied.
                if current.data.platform != parsed.platform {
                    warn!("'platform' changed (label/download_url_template/rss_key/torrent_dir); this needs a restart to take effect");
                }
                if current.data.clients != parsed.clients {
                    warn!("'clients' changed; this needs a restart to take effect");
                }
                // Same reason: the client is built once, the notification
                // backend is registered once, and the owner is read once. None
                // of it re-reads this table, so a change here looks applied and
                // is not.
                if current.data.telegram != parsed.telegram {
                    warn!("'telegram' changed; this needs a restart to take effect");
                }
                if current.data.slack != parsed.slack {
                    warn!("'slack' changed; this needs a restart to take effect");
                }
                // [notifications] is deliberately absent from this list: the
                // dispatcher and the poller read it through
                // `SharedNotificationOptions` on every wake-up, so the switches
                // apply at once and a changed backend is rebuilt in place.
            }

            let loaded = match LoadedOptions::from_data(parsed) {
                Ok(l) => l,
                Err(e) => {
                    error!("Reload rejected, keeping the running config: {e}");
                    reject(e.to_string());
                    return;
                }
            };

            match shared.lock() {
                Ok(mut guard) => {
                    *guard = loaded;
                    info!("Reloaded {}", path.display());
                }
                Err(e) => error!("Config lock poisoned, reload discarded: {e}"),
            }
        }

        /// Fallback nicknames for when the primary one is taken.
        ///
        /// `nick_1234` rather than `nick_`, `nick__`, `nick___`. Trailing
        /// underscores are a poor fallback: they are what every other client
        /// reaches for too, so a collision on the nick tends to be followed by a
        /// collision on the alternative -- and after a netsplit the bot's own
        /// ghost is often still holding `nick` while `nick_` is taken as well.
        ///
        /// Rolled once, when the config file is generated, so the nick stays
        /// stable across restarts. Re-rolling on every connect would make the
        /// bot unrecognisable in the channel.
        pub(crate) fn generate_alt_nicks(nickname: &str) -> Vec<String> {
            let mut suffixes: Vec<u32> = Vec::new();
            while suffixes.len() < 3 {
                let n = fastrand::u32(0..=10_000);
                // Distinct, or two of the three fallbacks would be one nick.
                if !suffixes.contains(&n) {
                    suffixes.push(n);
                }
            }
            suffixes.iter().map(|n| format!("{nickname}_{n}")).collect()
        }

        /// The IRC defaults, parsed from the annotated file that documents them.
        ///
        /// These used to be written twice -- once here, once in
        /// irc.defaults.toml -- with a test to catch the two drifting apart.
        /// They had already drifted once (`use_tls`), so the test was earning
        /// its keep. Embedding the file leaves nothing to drift, and makes the
        /// comments in it authoritative rather than aspirational.
        ///
        /// Note for the Dockerfile: `include_str!` reads at compile time, so
        /// irc.defaults.toml must be in the build context.
        fn get_irc_default_config() -> irc::client::data::config::Config {
            const DEFAULTS: &str = include_str!("../irc.defaults.toml");
            let mut cfg: irc::client::data::config::Config = toml::from_str(DEFAULTS)
                .expect("irc.defaults.toml is embedded at compile time and must parse");
            // Rolled per install so the nick stays stable across restarts; the
            // values in the file are only there to show the shape.
            cfg.alt_nicks =
                Self::generate_alt_nicks(cfg.nickname.as_deref().unwrap_or("irc2torrent"));
            cfg
        }

        pub fn is_commands_enabled(&self) -> bool {
            self.option_data.lock().unwrap().data.command_options.commands_enabled
        }

        /// Shared with the pacer so a change to the burst settings applies to
        /// the very next message rather than the next reconnect.
        pub fn flood_limit(&self) -> SharedFloodLimit {
            Arc::clone(&self.flood_limit)
        }

        pub fn requires_identified(&self) -> bool {
            self.option_data.lock().unwrap().data.command_options.require_identified
        }

        /// At least 2: a listing is a header plus entries, and a cap of 0 or 1
        /// would answer with a header and nothing else.
        pub fn max_reply_lines(&self) -> usize {
            self.option_data.lock().unwrap().data.command_options.max_reply_lines.max(2)
        }

        pub fn get_security_mode(&self) -> SecurityMode {
            self.option_data.lock().unwrap().data.command_options.security_mode.clone()
        }

        /// The client to drive: the **first** `[[clients]]` entry, and only it.
        ///
        /// Previously `.first().unwrap()`, which panicked on `clients = []` --
        /// the natural result of commenting the block out while switching
        /// clients -- with a bare "called `Option::unwrap()` on a `None` value"
        /// that named neither the file nor the setting.
        ///
        /// The warning matters today, not hypothetically: the config written on
        /// first run lists rTorrent *and* Flood, and the second has always been
        /// silently ignored.
        pub fn get_torrent_client(&mut self) -> Result<TorrentClientOption, Error> {
            let guard = self.option_data.lock().unwrap();
            let clients = &guard.data.clients;

            if clients.len() > 1 {
                warn!(
                    "{} [[clients]] entries are configured; only the first is used.",
                    clients.len()
                );
            }

            clients.first().cloned().ok_or_else(|| {
                Error::msg("options.toml has no [[clients]] entry; there is no client to drive")
            })
        }

        pub fn get_torrent_platform(&self) -> PlatformSection {
            self.option_data.lock().unwrap().data.platform.clone()
        }

        // The three regex accessors hand out clones of patterns compiled when the
        // config was loaded. They used to call Regex::new on every invocation --
        // once per download pattern for every announcement -- and `.unwrap()` on
        // a bad pattern, which turned a config typo into a crash mid-message.
        pub fn get_announce_regex(&self) -> Regex {
            self.option_data.lock().unwrap().announce_regex.clone()
        }

        pub fn get_dl_regexes(&self) -> Vec<Regex> {
            self.option_data.lock().unwrap().dl_regexes.clone()
        }

        pub fn get_reject_regexes(&self) -> Vec<Regex> {
            self.option_data.lock().unwrap().reject_regexes.clone()
        }

        pub fn get_telegram(&self) -> Option<TelegramOptions> {
            self.option_data.lock().unwrap().data.telegram.clone()
        }

        pub fn get_slack(&self) -> Option<SlackOptions> {
            self.option_data.lock().unwrap().data.slack.clone()
        }

        /// The Telegram user allowed to command the bot, or `None` when that
        /// transport is absent or has commands switched off.
        ///
        /// Read per command rather than captured, so turning `commands = false`
        /// in options.toml takes effect on the next message like every other
        /// setting in that file.
        pub fn telegram_owner(&self) -> Option<i64> {
            let guard = self.option_data.lock().unwrap();
            let t = guard.data.telegram.as_ref()?;
            t.commands.then_some(t.owner_id)
        }

        pub fn slack_owner(&self) -> Option<String> {
            let guard = self.option_data.lock().unwrap();
            let s = guard.data.slack.as_ref()?;
            s.commands.then(|| s.owner_id.clone())
        }

        pub fn get_notifications(&self) -> NotificationOptions {
            self.option_data.lock().unwrap().data.notifications.clone()
        }

        /// A handle the notification task can keep and re-read, rather than the
        /// startup snapshot it used to be given.
        pub fn shared_notifications(&self) -> SharedNotificationOptions {
            SharedNotificationOptions(self.option_data.clone())
        }

        /// Hand the config a live notifier once the backends exist.
        ///
        /// `&self` because the slot is behind its own lock: the watcher thread
        /// already holds a clone of the Arc, so this reaches it without the
        /// config itself having to be mutable.
        pub fn set_notifier(&self, notifier: crate::notify::Notifier) {
            match self.notifier.lock() {
                Ok(mut slot) => *slot = notifier,
                Err(e) => error!("Notifier slot poisoned: {e}"),
            }
        }

        /// The download patterns as written, in the order `remove_dl_regex`
        /// indexes them.
        ///
        /// Returns the raw strings rather than `dl_regexes.as_str()` so a listing
        /// shows what is actually in the file, and so it reads from the same vec
        /// the index refers to -- the compiled cache is derived from this one and
        /// could not drift, but there is no reason to route a listing through it.
        pub fn get_dl_patterns(&self) -> Vec<String> {
            self.option_data.lock().unwrap().data.regex_for_downloads_match.clone()
        }

        /// `&self`, not `&mut self`: `mutate_options` already takes `&self`, and
        /// the needless `&mut` forced callers to `borrow_mut()` the surrounding
        /// `RefCell` and hold that guard across the await below. A shared borrow
        /// cannot collide the same way.
        ///
        /// Returns an error rather than only logging one. Silently declining left
        /// the caller reporting "added to watch list" for a pattern that had been
        /// refused.
        pub async fn add_dl_regex(&self, regex: String) -> Result<(), Error> {
            // Validate before accepting: an unparseable pattern from a command
            // must not be written to the file or poison the compiled cache.
            if let Err(e) = Regex::new(&regex) {
                error!("Refusing to add an invalid regex '{regex}': {e}");
                return Err(Error::msg(format!("'{regex}' is not a valid regex: {e}")));
            }
            self.mutate_options(|data| data.regex_for_downloads_match.push(regex)).await;
            Ok(())
        }

        pub async fn remove_dl_regex(&self, index: usize) {
            self.mutate_options(|data| {
                if index < data.regex_for_downloads_match.len() {
                    data.regex_for_downloads_match.remove(index);
                }
            })
            .await;
        }

        /// Apply a change to the options, recompile, and persist.
        ///
        /// The recompile matters: the compiled regexes are what the accessors
        /// serve, so mutating `data` alone would leave them stale until the next
        /// file reload. The subsequent write does trigger the watcher, but the
        /// reload sees identical content and skips.
        async fn mutate_options<F>(&self, apply: F)
        where
            F: FnOnce(&mut OptionData),
        {
            let snapshot = {
                let mut guard = match self.option_data.lock() {
                    Ok(g) => g,
                    Err(e) => {
                        error!("Config lock poisoned, change not applied: {e}");
                        return;
                    }
                };
                apply(&mut guard.data);

                match LoadedOptions::from_data(guard.data.clone()) {
                    Ok(loaded) => {
                        let snapshot = loaded.data.clone();
                        *guard = loaded;
                        snapshot
                    }
                    Err(e) => {
                        error!("Change produced an invalid config, not saving: {e}");
                        return;
                    }
                }
            };

            let _ = self
                .update_option_file(OPTIONS_CONFIG_FILE.to_string(), &snapshot)
                .await;
        }

        /// The IRC config, with a send rate the servers actually tolerate.
        ///
        /// The irc crate defaults to 15 messages per 8-second window, and lets
        /// all 15 out *instantly* before it throttles anything. A real network's
        /// flood limit trips long before that: a multi-line listing got the bot
        /// killed for excess flood around the twentieth message, with the crate's
        /// own throttle only delaying the tail, far too late to help.
        ///
        /// So the burst is narrowed unless the operator set it themselves --
        /// 5 per 8s, roughly a message every 1.6s sustained. An explicit value in
        /// irc.toml always wins; this only replaces the crate's default.
        pub fn get_irc_config(&self) -> irc::client::data::Config {
            let mut cfg = self.irc_data.clone();
            if cfg.max_messages_in_burst.is_none() {
                cfg.max_messages_in_burst = Some(5);
            }
            if cfg.burst_window_length.is_none() {
                cfg.burst_window_length = Some(8);
            }
            cfg
        }

        async fn read_or_create_toml<T>(filename: String, data: Option<&T>) -> Option<T>
        where
            T: ser::Serialize,
            T: de::DeserializeOwned,
        {
            if let Some(full_path_buf) = Config::get_full_config_path(filename.clone()) {
                info!(
                    "You can edit the config file at '{}' location",
                    full_path_buf.to_str()?
                );
                debug!(
                    "You can edit the config file at '{}' location",
                    full_path_buf.to_str()?
                );
                return if full_path_buf.exists() {
                    let path = full_path_buf.as_path();
                    Self::read_file_to_toml::<T>(path).await
                } else {
                    // The result of this match used to be discarded by a stray
                    // semicolon, so the branch always fell through to `None`:
                    // the file was written correctly and then reported as a
                    // failure, making the very first run panic at
                    // `Config::new().await.unwrap()`. Under the supervisor that
                    // was masked by the restart, since the file exists by the
                    // second attempt.
                    let Some(result) = data else {
                        error!("No defaults available to create {}", full_path_buf.display());
                        return None;
                    };

                    // Pretty, so the regex lists come out one entry per line
                    // rather than as a single unreadable row -- these files are
                    // meant to be read and edited by hand.
                    let toml = match toml::to_string_pretty(result) {
                        Ok(t) => t,
                        Err(e) => {
                            error!("Could not serialise defaults for {}: {e}", full_path_buf.display());
                            return None;
                        }
                    };

                    let path = full_path_buf.as_path();
                    match fs::write(path, toml).await {
                        Ok(_) => {
                            info!("New options file created at '{}' location, please consider modifying it before running to app.", path.to_str()?);
                            Self::read_file_to_toml::<T>(path).await
                        }
                        Err(e) => {
                            error!("Error creating {} file: {e}", path.to_str()?);
                            None
                        }
                    }
                };
            }

            return None;
        }

        async fn read_file_to_toml<T>(path: &Path) -> Option<T>
        where
            T: ser::Serialize,
            T: de::DeserializeOwned,
        {
            let contents: String = match fs::read_to_string(path).await {
                Ok(c) => c,
                Err(_) => {
                    error!("Could not read file `{}`", path.to_str()?);
                    return None;
                }
            };
            match toml::from_str(&contents) {
                Ok(d) => d,
                Err(e) => {
                    // Carry the serde error. Without it a schema mismatch reports
                    // only "Unable to load data from <path>", which says nothing
                    // about *which* field is wrong -- and this is the message an
                    // upgrading user meets when their options.toml predates a
                    // schema change.
                    error!("Unable to load data from `{}`: {e}", path.to_str()?);
                    return None;
                }
            }
        }

        fn get_full_config_path(filename: String) -> Option<PathBuf> {
            if let Some(proj_dir) = BaseDirs::new() {
                let dir = proj_dir.config_dir();
                let full_path_buf = dir.join(filename);
                return Some(full_path_buf);
            }
            return None;
        }

        pub async fn update_option_file<T>(
            &self,
            filename: String,
            config: T,
        ) -> Result<bool, String>
        where
            T: ser::Serialize,
        {
            // Pretty for the same reason as above: this is the path that
            // rewrites options.toml after cmd:addtowatchlist / cmd:removewatch,
            // and it used to collapse the whole regex list onto one line.
            if let Ok(toml) = toml::to_string_pretty(&config) {
                if let Some(path) = Config::get_full_config_path(filename) {
                    return match fs::write(path, toml).await {
                        Ok(_) => {
                            info!("Options file updated");
                            Ok(true)
                        }
                        _ => {
                            error!("Error updating options file");
                            Err("Could not update options file".to_string())
                        }
                    };
                };
            } else {
                error!("Error updating options file");
                return Err("Could not update options file".to_string());
            }
            return Err("Could not update options file".to_string());
        }
    }

    /// The shipped defaults plus a working tracker section.
    ///
    /// `OptionData::default()` deliberately does NOT validate -- it has an empty
    /// `download_url_template`, which is the whole point of the startup error.
    /// Every test that needs a `LoadedOptions` therefore has to start from this
    /// instead, or `from_data` rejects it.
    #[cfg(test)]
    pub(crate) fn option_data_for_test() -> OptionData {
        OptionData {
            platform: PlatformSection {
                label: "ExampleTracker".to_string(),
                options: PlatformOptions {
                    download_url_template:
                        "https://tracker.example.org/rss/download/{id}/{key}/{file}".to_string(),
                    rss_key: "XXXXXXXX".to_string(),
                    torrent_dir: "/downloads/.torrents".to_string(),
                },
            },
            ..OptionData::default()
        }
    }

    /// Constructors used only by tests, so the real API does not have to expose
    /// a way to build a Config without a file or mutate it after the fact.
    #[cfg(test)]
    impl Config {
        pub fn default_for_test() -> Self {
            Self {
                option_data: Arc::new(Mutex::new(
                    LoadedOptions::from_data(option_data_for_test()).unwrap(),
                )),
                notifier: Arc::new(Mutex::new(crate::notify::Notifier::disabled())),
                flood_limit: Arc::new(Mutex::new(FloodLimit::from_irc(
                    &Config::get_irc_default_config(),
                ))),
                irc_data: Config::get_irc_default_config(),
                _watcher: None,
            }
        }

        pub fn set_for_test(&mut self, mode: SecurityMode, commands_enabled: bool) {
            let mut guard = self.option_data.lock().unwrap();
            guard.data.command_options.security_mode = mode;
            guard.data.command_options.commands_enabled = commands_enabled;
        }

        pub fn set_telegram_for_test(&mut self, owner_id: i64, commands: bool) {
            self.option_data.lock().unwrap().data.telegram = Some(TelegramOptions {
                token: "123:test".into(),
                owner_id,
                commands,
                notifications: true,
                events: EventFilter::default(),
            });
        }

        /// Write the section the way a reload would, so a test can prove the
        /// dispatcher notices.
        pub fn set_notifications_for_test(&mut self, notifications: NotificationOptions) {
            self.option_data.lock().unwrap().data.notifications = notifications;
        }

        pub fn set_slack_for_test(&mut self, owner_id: &str, commands: bool) {
            self.option_data.lock().unwrap().data.slack = Some(SlackOptions {
                app_token: "xapp-test".into(),
                bot_token: "xoxb-test".into(),
                channel_id: Some("C0TEST".into()),
                owner_id: owner_id.to_string(),
                commands,
                notifications: true,
                events: EventFilter::default(),
            });
        }
    }

    #[cfg(test)]
    mod test {
        use super::*;

        fn data_with(announce: &str, dl: Vec<&str>, reject: Vec<&str>) -> OptionData {
            OptionData {
                regex_for_announce_match: announce.to_string(),
                regex_for_downloads_match: dl.into_iter().map(String::from).collect(),
                regex_for_downloads_reject_match: reject.into_iter().map(String::from).collect(),
                // ..option_data_for_test(), not ..OptionData::default(): these
                // cases are about the regexes, and the shipped default has no
                // download_url_template, which from_data rejects first.
                ..option_data_for_test()
            }
        }

        #[test]
        fn valid_patterns_compile() {
            let loaded = LoadedOptions::from_data(data_with(
                r"Name:'(?P<name>.*)'.*/torrent/(?P<id>\d+)",
                vec![".*1080p.*", ".*2160p.*"],
                vec![".*GERMAN.*"],
            ))
            .expect("valid patterns should load");

            assert_eq!(loaded.dl_regexes.len(), 2);
            assert_eq!(loaded.reject_regexes.len(), 1);
            assert!(loaded.announce_regex.is_match("Name:'Thing' uploaded /torrent/12345"));
        }

        // A bad pattern must be caught when the config is read. Previously the
        // accessors called Regex::new per message and `.unwrap()`ed, so a typo
        // in the config crashed the bot on the next announcement instead.
        // `let ... else` rather than expect_err: that would require Debug on
        // LoadedOptions, and the struct holds the whole config -- not something
        // worth making printable just to satisfy a test.
        #[test]
        fn an_invalid_announce_regex_is_rejected() {
            let Err(err) = LoadedOptions::from_data(data_with("(unclosed", vec![], vec![])) else {
                panic!("invalid announce regex must be rejected");
            };
            assert!(err.to_string().contains("regex_for_announce_match"), "{err}");
        }

        #[test]
        fn an_invalid_download_regex_is_rejected() {
            let Err(err) = LoadedOptions::from_data(data_with(".*", vec!["*bad("], vec![])) else {
                panic!("invalid download regex must be rejected");
            };
            assert!(err.to_string().contains("regex_for_downloads_match"), "{err}");
        }

        #[test]
        fn an_invalid_reject_regex_is_rejected() {
            let Err(err) = LoadedOptions::from_data(data_with(".*", vec![], vec!["["])) else {
                panic!("invalid reject regex must be rejected");
            };
            assert!(err.to_string().contains("regex_for_downloads_reject_match"), "{err}");
        }

        /// The defaults are what gets written on first run, so they have to
        /// survive a serialise/parse round trip.
        ///
        /// The trap is TOML's ordering rule: a bare key cannot follow a table
        /// header, so any struct field that serialises as a table must be
        /// declared after every scalar. Getting that wrong produces a config file
        /// the bot writes and then cannot read.
        // ---- the platform section -----------------------------------------

        /// The test that makes the README true: any label parses.
        #[test]
        fn a_platform_table_with_any_label_parses() {
            let text = r#"
                [AnythingAtAll]
                download_url_template = "https://tracker.example.org/dl/{id}"
                torrent_dir = "/downloads/.torrents"
            "#;
            let parsed: PlatformSection = toml::from_str(text).expect("any label must parse");
            assert_eq!(parsed.label, "AnythingAtAll");
            assert_eq!(parsed.options.torrent_dir, "/downloads/.torrents");
            // rss_key is optional -- not every tracker has one.
            assert_eq!(parsed.options.rss_key, "");
        }

        #[test]
        fn the_platform_label_round_trips_through_toml() {
            let original = PlatformSection {
                label: "AnythingAtAll".to_string(),
                options: PlatformOptions {
                    download_url_template: "https://tracker.example.org/dl/{id}/{key}".to_string(),
                    rss_key: "SECRET".to_string(),
                    torrent_dir: "/downloads/.torrents".to_string(),
                },
            };
            let text = toml::to_string(&original).expect("must serialise");
            assert!(text.contains("[AnythingAtAll]"), "{text}");
            let parsed: PlatformSection = toml::from_str(&text).expect("and parse back");
            assert_eq!(parsed, original);
        }

        #[test]
        fn a_missing_platform_table_is_reported() {
            let err = toml::from_str::<PlatformSection>("").unwrap_err().to_string();
            assert!(err.contains("[platform."), "{err}");
        }

        #[test]
        fn two_platform_tables_are_rejected() {
            let text = r#"
                [First]
                download_url_template = "https://a.example/{id}"
                torrent_dir = "/t"
                [Second]
                download_url_template = "https://b.example/{id}"
                torrent_dir = "/t"
            "#;
            let err = toml::from_str::<PlatformSection>(text).unwrap_err().to_string();
            assert!(err.contains("First") && err.contains("Second"), "{err}");
        }

        /// The "no tracker configured" path, and the guard that stops a tracker
        /// default from creeping back into the shipped config.
        #[test]
        fn an_unconfigured_platform_is_rejected_with_the_field_name() {
            let Err(err) = LoadedOptions::from_data(OptionData::default()) else {
                panic!("the shipped default must NOT be usable as-is");
            };
            let msg = err.to_string();
            assert!(msg.contains("download_url_template"), "{msg}");
        }

        #[test]
        fn a_template_using_key_without_an_rss_key_is_rejected() {
            let mut data = option_data_for_test();
            data.platform.options.rss_key = String::new();
            let Err(err) = LoadedOptions::from_data(data) else {
                panic!("{{key}} with an empty rss_key must be rejected");
            };
            assert!(err.to_string().contains("rss_key"), "{err}");
        }

        #[test]
        fn an_empty_torrent_dir_is_rejected() {
            let mut data = option_data_for_test();
            data.platform.options.torrent_dir = String::new();
            let Err(err) = LoadedOptions::from_data(data) else {
                panic!("an empty torrent_dir must be rejected");
            };
            assert!(err.to_string().contains("torrent_dir"), "{err}");
        }

        /// The three regex tests below assert on regex field names, so the
        /// regexes have to be validated before the platform is.
        #[test]
        fn regex_errors_are_reported_before_platform_errors() {
            let mut data = option_data_for_test();
            data.regex_for_announce_match = "(unclosed".to_string();
            data.platform.options.download_url_template = String::new();
            let Err(err) = LoadedOptions::from_data(data) else {
                panic!("an invalid regex must be rejected");
            };
            let msg = err.to_string();
            assert!(msg.contains("regex_for_announce_match"), "{msg}");
            assert!(!msg.contains("download_url_template"), "{msg}");
        }

        /// Requirement, previously untested: the download key must never reach a
        /// log line. Modelled on transport_tokens_are_redacted_from_debug_output.
        #[test]
        fn the_rss_key_is_redacted_from_debug_output() {
            let data = option_data_for_test();
            let dumped = format!("{:?}", data.platform);
            assert!(!dumped.contains("XXXXXXXX"), "rss_key leaked into Debug: {dumped}");
            assert!(dumped.contains("<redacted>"), "{dumped}");
            // The template is not a secret and is the useful part of the dump.
            assert!(dumped.contains("tracker.example.org"), "{dumped}");
        }

        #[test]
        fn the_default_options_round_trip_through_toml() {
            let original = OptionData::default();
            let text = toml::to_string(&original).expect("defaults must serialise");
            let parsed: OptionData = toml::from_str(&text).expect("and parse back");
            assert_eq!(parsed, original, "round trip changed the config:\n{text}");
        }

        /// The irc crate lets 15 messages out as an instant burst before it
        /// throttles anything, which is what got the bot killed for excess
        /// flood. A narrower burst is substituted unless the operator chose one.
        #[test]
        fn the_send_burst_is_narrowed_but_never_overridden() {
            let cfg = Config::default_for_test();
            let effective = cfg.get_irc_config();
            assert_eq!(effective.max_messages_in_burst, Some(5));
            assert_eq!(effective.burst_window_length, Some(8));

            // An explicit choice in irc.toml wins.
            let mut explicit = Config::default_for_test();
            explicit.irc_data.max_messages_in_burst = Some(20);
            assert_eq!(explicit.get_irc_config().max_messages_in_burst, Some(20));
        }

        /// The embedded irc.defaults.toml must parse, and must ship no tracker.
        ///
        /// This replaces a test that compared the file against a second copy of
        /// the same values in code. They had drifted once already -- the file
        /// said `use_tls = false` while the generated config omitted it, and the
        /// crate reads a missing `use_tls` as "use TLS", so a generated irc.toml
        /// spoke TLS to a plaintext port and could never connect. The file is
        /// now `include_str!`d, so there is no second copy to drift from and the
        /// remaining question is whether the defaults are neutral and sane.
        #[test]
        fn the_embedded_irc_defaults_are_neutral() {
            let generated = Config::get_irc_default_config();

            // No network is shipped: the user must name their own.
            assert_eq!(generated.server.as_deref(), Some(""), "a server is shipped");
            assert!(generated.channels.is_empty(), "a channel is shipped: {:?}", generated.channels);

            // TLS on by default, on the conventional TLS port. nick_password is
            // sent over this connection.
            assert_eq!(generated.use_tls, Some(true), "use_tls");
            assert_eq!(generated.port, Some(6697), "port");

            // The flood settings the pacer relies on.
            assert_eq!(generated.burst_window_length, Some(8));
            assert_eq!(generated.max_messages_in_burst, Some(5));

            // alt_nicks are rolled per install, not taken from the file.
            let shape = Regex::new(r"^irc2torrent_\d{1,5}$").unwrap();
            assert_eq!(generated.alt_nicks.len(), 3, "alt_nicks count");
            for nick in &generated.alt_nicks {
                assert!(shape.is_match(nick), "alt nick '{nick}' is not nickname_<number>");
            }

            // And the file on disk is the one that was embedded.
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("irc.defaults.toml");
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            let shipped: irc::client::data::config::Config = toml::from_str(&text)
                .unwrap_or_else(|e| panic!("irc.defaults.toml is not a valid irc config: {e}"));
            assert_eq!(shipped.server, generated.server);
            assert_eq!(shipped.channels, generated.channels);
        }

        #[test]
        fn alt_nicks_are_distinct_and_within_range() {
            for _ in 0..200 {
                let nicks = Config::generate_alt_nicks("bot");
                assert_eq!(nicks.len(), 3);

                let mut seen = std::collections::HashSet::new();
                for nick in &nicks {
                    // Two identical fallbacks would waste one of the three.
                    assert!(seen.insert(nick.clone()), "duplicate alt nick in {nicks:?}");

                    let suffix = nick.strip_prefix("bot_").expect("prefixed with the nickname");
                    let n: u32 = suffix.parse().expect("numeric suffix");
                    assert!(n <= 10_000, "{n} out of range");
                }
            }
        }

        /// Regex lists are edited by hand and rewritten by
        /// cmd:addtowatchlist / cmd:removewatch, so they must come back out one
        /// entry per line rather than as a single unreadable row.
        #[test]
        fn regex_lists_are_written_one_entry_per_line() {
            let text = toml::to_string_pretty(&OptionData::default()).unwrap();

            assert!(
                text.contains("regex_for_downloads_match = [\n"),
                "the list should open onto its own line:\n{text}"
            );
            for pattern in &OptionData::default().regex_for_downloads_match {
                assert!(
                    text.contains(&format!("    \"{pattern}\",\n")),
                    "'{pattern}' should be on its own indented line:\n{text}"
                );
            }
            // And it must still parse back.
            let parsed: OptionData = toml::from_str(&text).unwrap();
            assert_eq!(parsed, OptionData::default());
        }

        /// The burst settings are the one part of irc.toml that applies without
        /// a reconnect, so the defaults substituted for an older file must match
        /// what the pacer would otherwise be given.
        #[test]
        fn the_flood_limit_matches_the_substituted_defaults() {
            let cfg = Config::default_for_test();
            let effective = cfg.get_irc_config();
            let limit = *cfg.flood_limit().lock().unwrap();

            assert_eq!(limit.max_in_burst, effective.max_messages_in_burst.unwrap() as usize);
            assert_eq!(limit.window.as_secs(), effective.burst_window_length.unwrap() as u64);
        }

        /// A file predating these settings still gets a usable limit, and a
        /// zero would stall the queue rather than send freely.
        #[test]
        fn a_missing_or_zero_burst_still_yields_a_sane_limit() {
            let bare = irc::client::data::config::Config::default();
            let limit = FloodLimit::from_irc(&bare);
            assert_eq!(limit.max_in_burst, 5);
            assert_eq!(limit.window.as_secs(), 8);

            let zero = irc::client::data::config::Config {
                max_messages_in_burst: Some(0),
                ..Default::default()
            };
            assert_eq!(FloodLimit::from_irc(&zero).max_in_burst, 1, "0 would stall the queue");
        }

        /// A cap below 2 would answer with a header and nothing else.
        #[test]
        fn the_reply_line_cap_has_a_floor() {
            let mut cfg = Config::default_for_test();
            assert_eq!(cfg.max_reply_lines(), 12, "the shipped default");

            cfg.option_data.lock().unwrap().data.command_options.max_reply_lines = 0;
            assert_eq!(cfg.max_reply_lines(), 2);
        }

        /// The documented sample must actually be a valid config.
        ///
        /// Documentation rots silently: rename a field and the sample goes on
        /// looking plausible while being unusable. This fails the build instead.
        #[test]
        fn the_sample_config_is_valid() {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/options.sample.toml");
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

            let parsed: OptionData = toml::from_str(&text)
                .unwrap_or_else(|e| panic!("docs/options.sample.toml is not a valid config: {e}"));

            // And it must pass the same validation a live reload applies, so the
            // sample cannot ship a regex that would be rejected on load.
            LoadedOptions::from_data(parsed.clone()).expect("sample must pass validation");

            // Every backend is commented out, matching the sample's claim that
            // notifications are entirely opt-in.
            assert!(parsed.notifications.email.is_none());
            assert!(parsed.notifications.ntfy.is_none());
            assert!(parsed.notifications.irc.is_none());
            // The two-way transports are opt-in on the same terms.
            assert!(parsed.telegram.is_none());
            assert!(parsed.slack.is_none());
        }

        /// An options.toml written before notifications existed must keep
        /// working; a missing section is a default, not a parse error that would
        /// take the bot down on upgrade.
        #[test]
        fn a_config_without_a_notifications_section_still_parses() {
            let mut text = toml::to_string(&OptionData::default()).unwrap();
            // Drop everything from the notifications table onward, mimicking an
            // older file.
            if let Some(at) = text.find("[notifications]") {
                text.truncate(at);
            }
            assert!(!text.contains("notifications"), "the section should be gone");

            let parsed: OptionData = toml::from_str(&text).expect("old config must still parse");
            assert_eq!(parsed.notifications, NotificationOptions::default());
        }

        /// Each backend is off until its own table appears, so an untouched
        /// install sends nothing.
        #[test]
        fn no_backend_is_enabled_by_default() {
            let o = NotificationOptions::default();
            assert!(o.email.is_none());
            assert!(o.ntfy.is_none());
            assert!(o.irc.is_none());
        }

        /// The four lines the Slack how-to tells people to paste must be enough
        /// on their own: both roles on, every event inherited.
        ///
        /// `commands` and `notifications` default to *true*, which is the one
        /// place a missing `#[serde(default = ...)]` would silently disable a
        /// transport that looks configured.
        #[test]
        fn a_minimal_two_way_transport_table_enables_both_roles() {
            // Appended to a real default config: these are tables, and a table
            // has to follow every scalar or the rest of the file lands inside it.
            let text = toml::to_string(&OptionData::default()).unwrap()
                + r#"
[telegram]
token = "123:abc"
owner_id = 42

[slack]
app_token = "xapp-1"
bot_token = "xoxb-1"
channel_id = "C01234567"
owner_id = "U01234567"
"#;
            let parsed: OptionData = toml::from_str(&text).expect("must parse");

            let t = parsed.telegram.expect("telegram table present");
            assert!(t.commands && t.notifications, "both roles default on");
            assert_eq!(t.events, EventFilter::default(), "nothing overridden");

            let s = parsed.slack.expect("slack table present");
            assert!(s.commands && s.notifications, "both roles default on");
            assert_eq!(s.owner_id, "U01234567");
        }

        /// Turning off one role must not disturb the other -- "notifications
        /// only" is the documented way to run Slack without opening a socket.
        #[test]
        fn either_role_can_be_turned_off_alone() {
            let text = toml::to_string(&OptionData::default()).unwrap()
                + r#"
[slack]
app_token = "xapp-1"
bot_token = "xoxb-1"
channel_id = "C1"
owner_id = "U1"
commands = false
"#;
            let parsed: OptionData = toml::from_str(&text).expect("must parse");

            let s = parsed.slack.expect("slack table present");
            assert!(!s.commands);
            assert!(s.notifications, "the other role stays on");
        }

        /// The tokens are full credentials for the bot; a debug print of the
        /// config must not put them in the log.
        #[test]
        fn transport_tokens_are_redacted_from_debug_output() {
            let slack = SlackOptions {
                app_token: "xapp-secret".into(),
                bot_token: "xoxb-secret".into(),
                channel_id: Some("C1".into()),
                owner_id: "U1".into(),
                commands: true,
                notifications: true,
                events: EventFilter::default(),
            };
            let shown = format!("{slack:?}");
            assert!(!shown.contains("secret"), "{shown}");
            // The non-secret fields stay visible, or the output is useless.
            assert!(shown.contains("C1") && shown.contains("U1"), "{shown}");
        }

        /// A backend table with per-event overrides has to parse, since that is
        /// the shape the README tells people to write.
        #[test]
        fn a_backend_can_override_individual_events() {
            let text = r#"
                on_failure = true
                on_torrent_added = true

                [ntfy]
                topic = "some-unguessable-topic"

                [ntfy.events]
                on_torrent_added = false
            "#;
            let o: NotificationOptions = toml::from_str(text).expect("must parse");
            let ntfy = o.ntfy.expect("ntfy table present");

            assert_eq!(ntfy.topic, "some-unguessable-topic");
            // Stated: overridden. Unstated: left to inherit.
            assert_eq!(ntfy.events.on_torrent_added, Some(false));
            assert_eq!(ntfy.events.on_failure, None);
        }

        /// dxr resolves the SCGI socket from `Url::path()`, so the `unix:` URL
        /// must carry the socket path *as the path* -- with a single slash after
        /// the colon.
        ///
        /// Writing `unix://` starts an authority, so the first path segment is
        /// swallowed as a hostname: `unix://config/.local/...` asks for
        /// `/.local/...`, which does not exist, and the only symptom is a
        /// connection refused against a path nobody mentioned.
        #[test]
        fn the_unix_socket_url_must_not_use_a_double_slash() {
            let want = "/config/.local/share/rtorrent/rtorrent.sock";

            let correct = url::Url::parse("unix:/config/.local/share/rtorrent/rtorrent.sock").unwrap();
            assert_eq!(correct.path(), want);
            assert_eq!(correct.host_str(), None);

            let wrong = url::Url::parse("unix://config/.local/share/rtorrent/rtorrent.sock").unwrap();
            assert_eq!(wrong.host_str(), Some("config"), "the path segment became a host");
            assert_ne!(wrong.path(), want, "so the socket path is wrong");
        }

        /// The shipped default must be a form that actually resolves.
        #[test]
        fn the_default_socket_url_resolves_to_the_real_socket() {
            // Not via OptionData::default(), which now follows
            // IRC2TORRENT_DEFAULT_CLIENT and would depend on the environment the
            // test happens to run in.
            let opts = rTorrentOptions::default();
            let parsed = url::Url::parse(&opts.xmlrpc_url).unwrap();
            assert_eq!(parsed.path(), "/config/.local/share/rtorrent/rtorrent.sock");
            assert_eq!(parsed.host_str(), None);
        }

        /// A reload swaps the whole LoadedOptions, so everything in OptionData
        /// comes along -- not just the regexes. `commands_enabled` is checked
        /// here specifically because `is_commands_enabled()` gates the entire
        /// remote-command surface (auth.rs), and it is read fresh on every
        /// command rather than cached at startup.
        #[test]
        fn reload_picks_up_command_options() {
            let dir = std::env::temp_dir().join("irc2torrent-test-reload-cmdopts");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("options.toml");

            // Both sides need a valid platform section: reload_options runs the
            // parsed file back through from_data, so an unconfigured tracker
            // would be rejected and the reload would look like it did nothing.
            let mut running = option_data_for_test();
            running.command_options.commands_enabled = false;
            let shared = Arc::new(Mutex::new(LoadedOptions::from_data(running).unwrap()));

            let mut on_disk = option_data_for_test();
            on_disk.command_options.commands_enabled = true;
            on_disk.command_options.security_mode = SecurityMode::Password("s3cret".into());
            std::fs::write(&path, toml::to_string(&on_disk).unwrap()).unwrap();

            let notifier = Arc::new(Mutex::new(crate::notify::Notifier::disabled()));
            Config::reload_options(&path, &shared, &notifier);

            let guard = shared.lock().unwrap();
            assert!(
                guard.data.command_options.commands_enabled,
                "commands_enabled should have been picked up by the reload"
            );
            assert!(
                matches!(guard.data.command_options.security_mode, SecurityMode::Password(_)),
                "security_mode should have been picked up too"
            );
            drop(guard);
            let _ = std::fs::remove_file(&path);
        }

        /// The reload path skips when the parsed data equals what is running.
        /// That is what stops the bot's own writes (add_dl_regex etc.) from
        /// bouncing back through the watcher, so the equality really must hold.
        #[test]
        fn identical_data_compares_equal_and_a_change_does_not() {
            let a = data_with(".*", vec![".*1080p.*"], vec![]);
            assert_eq!(a, a.clone());

            let b = data_with(".*", vec![".*720p.*"], vec![]);
            assert_ne!(a, b);
        }
    }
}
