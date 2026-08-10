pub mod commands {
    use std::cell::RefCell;
    use std::rc::Rc;

    use log::{error, info};
    use pub_sub::{PubSub, Subscription};
    use regex::Regex;

    use crate::auth::{redact_secrets, Authorization, AuthResult, CommandRequest, Principal};
    use crate::clients::TorrentInfo;
    use crate::torrent_processor::torrent::TorrentProcessor;
    use crate::Config;

    #[derive(Debug, PartialEq, Eq)]
    pub enum CommandError {
        Unauthorized,
        NotImplemented(String),
        BadArguments(String),
        Failed(String),
    }

    impl std::fmt::Display for CommandError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                CommandError::Unauthorized => write!(f, "You are not authorized to use this bot."),
                CommandError::NotImplemented(c) => write!(f, "Command '{c}' is not implemented yet."),
                CommandError::BadArguments(m) => write!(f, "{m}"),
                CommandError::Failed(m) => write!(f, "{m}"),
            }
        }
    }

    /// What marks a message as a command.
    ///
    /// `command:` is accepted alongside `cmd:` because it is the obvious thing to
    /// type, and getting it wrong was **silent**: `is_command` returned false, so
    /// the message fell through to the announce regex, matched it, and was
    /// handled as an announcement -- meaning an explicit `addtorrent` request was
    /// quietly judged by the download lists it exists to bypass, and the log said
    /// only that the name matched no pattern.
    ///
    /// Anchored to the start of the message, unlike the bare `cmd:` pattern it
    /// replaces. Left unanchored, a release named something like
    /// `Command: Anthology` would route an ordinary channel announcement into the
    /// command handler, where a non-owner fails authorization and the bot answers
    /// the *channel* with "You are not authorized to use this bot."
    pub(crate) const COMMAND_PATTERN: &str =
        r"(?i)^\s*(?:cmd|command)\s*:\s*(?P<command>\w+)(?:\s+params\s*:\s*\((?P<params>.*)\))?";

    /// A bare torrent link or id, for `addtorrent` arguments that are not a full
    /// announce line: `https://tracker.example.org/torrent/241813706`,
    /// `/torrent/241813706`, `8f3c1a2b`, or just `241813706`.
    ///
    /// `\S*` rather than `.*` before the id, so the whole argument has to be one
    /// unbroken token. Otherwise an announce line would also match here -- its
    /// trailing URL ends in the id -- and a mis-written
    /// `regex_for_announce_match` would silently fall through to this branch and
    /// lose the release name instead of reporting itself.
    ///
    /// The id charset is deliberately wider than `\d+`: plenty of trackers use
    /// hashes or slugs, and restricting it to digits made the shortcut work on
    /// numeric-id sites only. The cost is that a single-word typo now looks like
    /// an id -- `cmd:addtorrent params:(pls)` used to answer with the usage
    /// line and now fetches `.../pls/...` and reports the tracker's 404. That is
    /// unavoidable without knowing the site's id format, and it is loud, bounded,
    /// and only reachable by an already-authorised user. Note also that a
    /// query-string link (`?id=abc`) still does not match, since `?` and `=` are
    /// outside the class -- paste the bare id for those.
    ///
    /// Whatever matches here is percent-encoded before it reaches a URL
    /// (`platforms::url_template`), so a wider charset cannot inject URL syntax.
    pub(crate) const BARE_ID_PATTERN: &str =
        r"^\s*(?:\S*/)?(?P<id>[A-Za-z0-9][A-Za-z0-9._-]*)/?\s*$";

    /// One command: its short form, its real name, and what it is for.
    ///
    /// Single source of truth. Expansion reads `short` and `full`; the help is
    /// rendered from the same rows, so a command cannot appear in one and not
    /// the other, and the help can never name a command that does not exist.
    pub(crate) struct CommandInfo {
        pub short: &'static str,
        pub full: &'static str,
        /// Argument placeholder, empty when the command takes none.
        pub arg: &'static str,
        pub help: &'static str,
    }

    /// Short forms are in the `at! <thing>` style most IRC bots use, and are
    /// systematic rather than arbitrary: `a` add, `l` list, `r` remove, with the
    /// second letter naming the subject -- `t` torrent, `w` watchlist.
    ///
    /// A short form is rewritten into the canonical `cmd:name params:(...)`
    /// before anything else looks at it, so there is still one parser, one
    /// command table and one authorization path. A shortcut cannot drift from
    /// the command it stands for, because by the time it runs it *is* that
    /// command.
    pub(crate) const COMMANDS: &[CommandInfo] = &[
        CommandInfo {
            short: "at",
            full: "addtorrent",
            arg: "<link | id | announce line>",
            help: "add a torrent, ignoring the filters",
        },
        CommandInfo {
            short: "lt",
            full: "torrentlist",
            arg: "",
            help: "list torrents: size, progress, ratio",
        },
        CommandInfo {
            short: "lw",
            full: "watchlist",
            arg: "",
            help: "list watch patterns, numbered",
        },
        CommandInfo {
            short: "aw",
            full: "addtowatchlist",
            arg: "<regex>",
            help: "add a watch pattern",
        },
        CommandInfo {
            short: "rw",
            full: "removewatch",
            arg: "<index>",
            help: "remove the watch pattern at that index",
        },
        CommandInfo {
            short: "tn",
            full: "testnotify",
            arg: "",
            help: "send a test notification",
        },
        CommandInfo {
            short: "s",
            full: "stop",
            arg: "",
            help: "discard replies still queued (alerts are kept)",
        },
        CommandInfo { short: "h", full: "help", arg: "", help: "this list" },
    ];

    /// A short form and whatever follows it.
    ///
    /// Anchored like COMMAND_PATTERN, for the same reason: unanchored, an
    /// announce line containing "Oh! " would start looking like a command.
    /// An unrecognised short form simply fails the table lookup and the message
    /// carries on to the announce matcher, so this cannot swallow announcements.
    const SHORTCUT_PATTERN: &str = r"(?i)^\s*(?P<short>[a-z]{1,4})!\s*(?P<rest>.*)$";

    /// Rewrite `at! <thing>` into `cmd:addtorrent params:(<thing>)`.
    ///
    /// `None` when the message is not a known short form, which includes every
    /// ordinary sentence that happens to contain an exclamation mark.
    pub(crate) fn expand_shortcut(msg: &str) -> Option<String> {
        let re = Regex::new(SHORTCUT_PATTERN).unwrap();
        let caps = re.captures(msg)?;

        let short = caps["short"].to_ascii_lowercase();
        let full = COMMANDS.iter().find(|c| c.short == short).map(|c| c.full)?;

        let rest = caps["rest"].trim();
        Some(if rest.is_empty() {
            format!("cmd:{full}")
        } else {
            // The canonical parser takes the *last* `)` as the delimiter, so a
            // parameter that is itself full of parentheses -- a regex, say --
            // survives being wrapped here.
            format!("cmd:{full} params:({rest})")
        })
    }

    /// How many bytes one reply line may carry.
    ///
    /// Each line is its own PRIVMSG, and the whole IRC line -- prefix, command,
    /// target, message, CRLF -- has to fit in 512 bytes. Anything past that the
    /// server truncates, silently and possibly mid-UTF-8. 380 leaves room for
    /// the envelope.
    const LINE_BUDGET: usize = 380;

    /// Trim one line to the per-message budget.
    ///
    /// Cuts on a char boundary: a torrent name is arbitrary UTF-8 from a
    /// `.torrent`, and slicing by byte offset would panic.
    fn fit_line(line: &str, budget: usize) -> String {
        if line.len() <= budget {
            return line.to_string();
        }
        let mut end = budget;
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &line[..end])
    }

    /// How much of a reply the asking transport can actually carry.
    ///
    /// Both limits are IRC's, and neither is a property of the command. A
    /// listing is capped because every line is a separate PRIVMSG and servers
    /// kill clients that send too many; each line is trimmed because the whole
    /// IRC line has to fit in 512 bytes.
    ///
    /// Telegram and Slack have neither problem -- 4096 characters, real
    /// newlines, no flood limit worth pacing at this volume -- and `chunk_lines`
    /// already bounds the message itself. Applying IRC's caps there would answer
    /// `lt!` with twelve torrents and "… and 30 more" for no reason at all.
    #[derive(Clone, Copy)]
    pub(crate) struct ReplyLimits {
        /// Lines a listing may span before the rest becomes "… and N more".
        max_lines: usize,
        /// Bytes one line may occupy, where the transport has such a limit.
        line_budget: Option<usize>,
    }

    impl ReplyLimits {
        fn irc(max_lines: usize) -> Self {
            Self { max_lines, line_budget: Some(LINE_BUDGET) }
        }

        /// A transport that takes a reply whole.
        fn unconstrained() -> Self {
            Self { max_lines: usize::MAX, line_budget: None }
        }
    }

    /// Cap a listing, appending a line naming what was left out.
    fn cap_lines(mut lines: Vec<String>, limits: ReplyLimits) -> Vec<String> {
        if lines.len() > limits.max_lines {
            let dropped = lines.len() - limits.max_lines;
            lines.truncate(limits.max_lines);
            lines.push(format!("… and {dropped} more (not shown)"));
        }
        match limits.line_budget {
            Some(budget) => lines.iter().map(|l| fit_line(l, budget)).collect(),
            None => lines,
        }
    }

    /// What each command does, one line each, in both spellings.
    ///
    /// Both forms are shown because the short one is the whole point but the
    /// long one is what the README, the logs and the error messages use -- a
    /// help listing only the abbreviations would leave `cmd:addtowatchlist`
    /// looking like a different feature.
    ///
    /// Rendered from COMMANDS rather than written out, so it cannot fall behind
    /// the table it documents. Columns are padded to the widest entry so the
    /// result lines up in a monospace client.
    pub(crate) fn help_lines(limits: ReplyLimits) -> Vec<String> {
        let rows: Vec<(String, String, &str)> = COMMANDS
            .iter()
            .map(|c| {
                let short = if c.arg.is_empty() {
                    format!("{}!", c.short)
                } else {
                    format!("{}! {}", c.short, c.arg)
                };
                let long = if c.arg.is_empty() {
                    format!("cmd:{}", c.full)
                } else {
                    format!("cmd:{} params:(…)", c.full)
                };
                (short, long, c.help)
            })
            .collect();

        let short_width = rows.iter().map(|(s, _, _)| s.chars().count()).max().unwrap_or(0);
        let long_width = rows.iter().map(|(_, l, _)| l.chars().count()).max().unwrap_or(0);

        let mut lines: Vec<String> = rows
            .iter()
            .map(|(s, l, h)| format!("{s:<short_width$}  {l:<long_width$}  — {h}"))
            .collect();

        // Aliases exist mainly so the obvious guess works; worth one line, not
        // a column of their own.
        lines.push("aliases: downloadlist = torrentlist, listwatch = watchlist, commands = help"
            .to_string());

        // Capped like every other listing, where the transport needs it. An
        // operator who lowered max_reply_lines to protect a strict network
        // should not be flooded by the help of all things.
        cap_lines(lines, limits)
    }

    /// Render a download list, one torrent per line.
    ///
    /// A header first, so a listing that gets cut still states the real total.
    pub(crate) fn format_download_list(list: &[TorrentInfo], limits: ReplyLimits) -> Vec<String> {
        if list.is_empty() {
            return vec!["No torrents.".to_string()];
        }

        let done = list.iter().filter(|t| t.is_complete()).count();
        let mut lines = vec![format!(
            "{} torrent{} ({done} complete):",
            list.len(),
            if list.len() == 1 { "" } else { "s" }
        )];
        lines.extend(list.iter().enumerate().map(|(i, t)| format!("[{i}] {}", t.summary())));
        cap_lines(lines, limits)
    }

    /// Render the watchlist, one pattern per line, numbered for `removewatch`.
    ///
    /// The indices come from the full list, so they stay correct even when the
    /// listing is cut short. Renumbering only the shown subset would make
    /// `cmd:removewatch` delete a different pattern than the one displayed.
    pub(crate) fn format_watchlist(patterns: &[String], limits: ReplyLimits) -> Vec<String> {
        if patterns.is_empty() {
            return vec![
                "Watchlist is empty. Add one with cmd:addtowatchlist params:(<regex>)".to_string(),
            ];
        }

        let mut lines = vec![format!(
            "{} pattern{}:",
            patterns.len(),
            if patterns.len() == 1 { "" } else { "s" }
        )];
        lines.extend(patterns.iter().enumerate().map(|(i, p)| format!("[{i}] {p}")));
        cap_lines(lines, limits)
    }

    pub struct CommandProcessor {
        evt_channel: PubSub<String>,
        subs_cfg: Vec<Subscription<String>>,
        config: Rc<RefCell<Config>>,
        tp: Rc<TorrentProcessor>,
        command_catching_regex: Regex,
        bare_id_regex: Regex,
        authorizer: Authorization,
        notifier: crate::notify::Notifier,
        /// The live IRC connection, so `stop` can reach the outbound queue.
        irc_link: crate::notify::IrcSenderSlot,
    }

    impl CommandProcessor {
        pub fn new(cfg: Rc<RefCell<Config>>, torrent_processor: Rc<TorrentProcessor>, evt_channel: PubSub<String>, subs_cfg: Vec<Subscription<String>>, notifier: crate::notify::Notifier, irc_link: crate::notify::IrcSenderSlot) -> Self {
            Self {
                config: cfg.clone(),
                command_catching_regex: Regex::new(COMMAND_PATTERN).unwrap(),
                bare_id_regex: Regex::new(BARE_ID_PATTERN).unwrap(),
                authorizer: Authorization::new(cfg.clone()),
                notifier,
                irc_link,
                tp: torrent_processor,
                evt_channel,
                subs_cfg,
            }
        }

        pub fn is_command(&self, msg: &str) -> bool {
            self.command_catching_regex.is_match(msg) || expand_shortcut(msg).is_some()
        }

        /// Run a command on behalf of `origin`.
        ///
        /// Authorization is enforced *here* rather than being left to the caller.
        /// The `authorizer` field previously existed but was never consulted, so
        /// the only check lived in `IrcProcessor`; any other call site would have
        /// executed commands unauthenticated.
        pub async fn process_command(
            &self,
            request: &CommandRequest,
        ) -> Result<Vec<String>, CommandError> {
            if let AuthResult::NotAuthorized = self.authorizer.authorize_command(request) {
                return Err(CommandError::Unauthorized);
            }
            let message = request.text.clone();
            let limits = self.reply_limits(&request.principal);

            // Expand a short form first, so everything below sees one syntax.
            // Authorization has already run against the message as sent, which
            // is what the redaction and the audit log record.
            let message = expand_shortcut(&message).unwrap_or(message);

            let Some(caps) = self.command_catching_regex.captures(message.as_str()) else {
                return Err(CommandError::BadArguments("Not a command.".to_string()));
            };

            // Lowercased because the pattern is case-insensitive: without this,
            // `CMD:AddTorrent` would parse cleanly and then fall through to
            // NotImplemented.
            let command = caps["command"].to_ascii_lowercase();
            // The params group is optional and `is_command` matches a bare
            // "cmd:foo", so indexing it directly panicked on every argument-less
            // command (e.g. "cmd:torrentlist").
            let argument = caps
                .name("params")
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();

            info!("Command: {}", command);
            info!("Argument: {}", redact_secrets(&argument));

            let result = match command.as_str() {
                "addtorrent" => self.add_torrent(&argument).await,
                "torrentlist" | "downloadlist" => self.torrent_list(limits).await,
                "watchlist" | "listwatch" => Ok(self.watch_list(limits)),
                "help" | "commands" => Ok(help_lines(limits)),
                "stop" | "cancel" => Ok(self.stop_queued()),
                "testnotify" => {
                    // Notification setup fails silently otherwise -- a wrong SMTP
                    // password produces nothing at all -- so this exists to make
                    // it answerable in one message.
                    self.notifier.send(crate::notify::Event::Test);
                    Ok(vec!["Test notification queued; check your configured targets. \
                             If nothing arrives, the log names the reason."
                        .to_string()])
                }
                "addtowatchlist" => self.add_torrent_to_watchlist(&argument).await,
                "removewatch" => match argument.trim().parse::<usize>() {
                    Ok(idx) => self.remove_watch(idx).await,
                    Err(_) => Err(CommandError::BadArguments(
                        "Use: cmd:removewatch params:(<index>)".to_string(),
                    )),
                },
                other => Err(CommandError::NotImplemented(other.to_string())),
            };

            self.log_result(result)
        }

        fn log_result(
            &self,
            result: Result<Vec<String>, CommandError>,
        ) -> Result<Vec<String>, CommandError> {
            match &result {
                Ok(lines) => info!("{}", lines.join(" | ")),
                Err(e) => error!("{}", e),
            }
            result
        }

        async fn remove_watch(&self, idx: usize) -> Result<Vec<String>, CommandError> {
            self.tp
                .remove_torrent_from_watchlist(idx)
                .await
                .map(|s| vec![s])
                .map_err(CommandError::Failed)
        }

        async fn add_torrent(&self, argument: &str) -> Result<Vec<String>, CommandError> {
            // Bind the regex to a local so the RefCell guard from `borrow()` is
            // dropped before the await below. Holding it across the await made a
            // concurrent `borrow_mut()` panic.
            let announce_regex = self.config.borrow().get_announce_regex();

            let (name, id) = if let Some(caps) = announce_regex.captures(argument) {
                (caps["name"].to_string(), caps["id"].to_string())
            } else if let Some(caps) = self.bare_id_regex.captures(argument) {
                // A link on its own carries no release name. That only costs a
                // label: the name inside the .torrent is what the client shows,
                // and Flood ignores this argument entirely. It names the cached
                // .torrent file, the last (cosmetic) segment of the tracker's
                // download URL, and the log line.
                let id = caps["id"].to_string();
                (format!("torrent-{id}"), id)
            } else {
                return Err(CommandError::BadArguments(
                    "Use: cmd:addtorrent params:(<announce line, torrent link, or id>)".to_string(),
                ));
            };

            self.tp
                .add_torrent(&name, &id)
                .await
                .map(|s| vec![s])
                .map_err(CommandError::Failed)
        }

        /// What the asking transport can carry.
        ///
        /// Only IRC needs limiting, and only because of IRC: the 512-byte line
        /// and the flood limit that made `max_reply_lines` necessary in the
        /// first place. Applying them to Telegram or Slack would trim a reply
        /// that fits comfortably in one message.
        fn reply_limits(&self, principal: &Principal) -> ReplyLimits {
            match principal {
                Principal::Irc { .. } => {
                    ReplyLimits::irc(self.config.borrow().max_reply_lines())
                }
                Principal::Telegram { .. } | Principal::Slack { .. } => {
                    ReplyLimits::unconstrained()
                }
            }
        }

        async fn torrent_list(&self, limits: ReplyLimits) -> Result<Vec<String>, CommandError> {
            let list = self
                .tp
                .get_download_list()
                .await
                .map_err(|e| CommandError::Failed(e.to_string()))?;

            Ok(format_download_list(&list, limits))
        }

        /// Throw away replies still waiting in the send queue.
        ///
        /// A long listing takes about a minute to arrive at the shipped rate, and
        /// until now there was no way to call it off. This bumps the queue's
        /// cancellation epoch, so everything already queued is discarded on its
        /// way out rather than sent.
        ///
        /// It cancels *all* outstanding replies, not just the most recent
        /// request: if two listings are still draining, "stop" plainly means
        /// both. Notifications are queued as uninterruptible and survive -- being
        /// told to stop listing torrents is not a request to throw away a
        /// download-finished alert waiting behind it.
        ///
        /// This reply is queued *after* the bump, so it is not cancelled by it.
        fn stop_queued(&self) -> Vec<String> {
            let dropped = match self.irc_link.lock() {
                Ok(link) => link.sender.as_ref().map(|q| q.cancel()),
                Err(e) => {
                    error!("IRC link lock poisoned: {e}");
                    None
                }
            };

            match dropped {
                Some(0) | None => vec!["Nothing was queued.".to_string()],
                Some(1) => vec!["Stopped; 1 queued message discarded.".to_string()],
                Some(n) => vec![format!("Stopped; {n} queued messages discarded.")],
            }
        }

        /// The watchlist, numbered so the indices can be fed to `removewatch`,
        /// which until now took an index nothing ever showed.
        fn watch_list(&self, limits: ReplyLimits) -> Vec<String> {
            format_watchlist(&self.tp.get_watchlist(), limits)
        }

        async fn add_torrent_to_watchlist(&self, argument: &str) -> Result<Vec<String>, CommandError> {
            if argument.trim().is_empty() {
                return Err(CommandError::BadArguments(
                    "Use: cmd:addtowatchlist params:(<regex>)".to_string(),
                ));
            }
            self.tp
                .add_torrent_to_watchlist(argument.to_owned())
                .await
                .map(|s| vec![s])
                .map_err(CommandError::Failed)
        }
    }

    #[cfg(test)]
    mod test {
        use super::*;

        /// Parses with the real pattern. The previous routing test kept its own
        /// copy of the regex literal, so it could not have caught a message the
        /// production one rejected.
        fn parse(msg: &str) -> Option<(String, String)> {
            let caps = Regex::new(COMMAND_PATTERN).unwrap().captures(msg)?;
            Some((
                caps["command"].to_ascii_lowercase(),
                caps.name("params").map(|m| m.as_str().to_string()).unwrap_or_default(),
            ))
        }

        const ANNOUNCE: &str = "New Torrent Announcement: <Movies :: Bluray>  Name:'The \
             Conversation 1974 1080p BluRay REMUX DTS-HD MA 5 1-d3g' uploaded by 'Anonymous' \
             freeleech - https://tracker.example.org/torrent/241813706";

        /// The spelling that silently did nothing. "command:" contains no "cmd:"
        /// substring, so the old pattern missed it, the message fell through to
        /// the announce regex, matched, and was handled as an announcement --
        /// filtered by the very download lists an explicit command bypasses.
        #[test]
        fn the_spelled_out_prefix_is_a_command() {
            let msg = format!("command:addtorrent params:({ANNOUNCE})");
            let (command, params) = parse(&msg).expect("should be recognised as a command");
            assert_eq!(command, "addtorrent");
            assert!(params.contains("The Conversation"), "{params}");
        }

        #[test]
        fn the_short_prefix_still_works() {
            let msg = format!("cmd:addtorrent params:({ANNOUNCE})");
            assert_eq!(parse(&msg).unwrap().0, "addtorrent");
        }

        #[test]
        fn the_prefix_and_command_are_case_insensitive() {
            let msg = format!("CMD:AddTorrent PARAMS:({ANNOUNCE})");
            assert_eq!(parse(&msg).unwrap().0, "addtorrent");
        }

        #[test]
        fn a_command_without_parameters_parses() {
            assert_eq!(parse("cmd:torrentlist"), Some(("torrentlist".into(), String::new())));
        }

        /// Why the pattern is anchored: a release whose name contains "Command:"
        /// must stay an announcement. Routed to the command handler it would fail
        /// authorization, and the bot would tell the whole channel "You are not
        /// authorized to use this bot."
        #[test]
        fn an_announce_line_mentioning_a_command_is_not_one() {
            let msg = "New Torrent Announcement: Name:'Command: Anthology 1080p' uploaded by \
                       'Anon' - https://tracker.example.org/torrent/1";
            assert_eq!(parse(msg), None);
        }

        #[test]
        fn leading_whitespace_is_tolerated() {
            assert_eq!(parse("  cmd:torrentlist").unwrap().0, "torrentlist");
        }

        fn bare_id(argument: &str) -> Option<String> {
            Regex::new(BARE_ID_PATTERN)
                .unwrap()
                .captures(argument)
                .map(|c| c["id"].to_string())
        }

        #[test]
        fn a_bare_link_yields_the_torrent_id() {
            assert_eq!(
                bare_id("https://tracker.example.org/torrent/241813706"),
                Some("241813706".into())
            );
        }

        #[test]
        fn a_trailing_slash_and_surrounding_space_are_tolerated() {
            assert_eq!(
                bare_id("  https://tracker.example.org/torrent/241813706/  "),
                Some("241813706".into())
            );
        }

        #[test]
        fn a_bare_id_is_accepted_on_its_own() {
            assert_eq!(bare_id("241813706"), Some("241813706".into()));
        }

        /// Plenty of trackers do not use numeric ids. Restricting the pattern to
        /// `\d+` quietly made the `at!` shortcut numeric-sites-only.
        #[test]
        fn a_hash_id_link_yields_the_id() {
            assert_eq!(
                bare_id("https://tracker.example.org/torrent/8f3c1a2b"),
                Some("8f3c1a2b".into())
            );
            assert_eq!(bare_id("8f3c1a2b"), Some("8f3c1a2b".into()));
        }

        #[test]
        fn a_slug_id_is_accepted() {
            assert_eq!(
                bare_id("https://tracker.example.org/t/some-release.2024"),
                Some("some-release.2024".into())
            );
        }

        /// The safety property the wider charset must not cost us: whatever the
        /// id may contain, an argument with whitespace in it is still not one.
        #[test]
        fn a_widened_id_still_does_not_claim_an_announce_line() {
            assert_eq!(bare_id("8f3c1a2b and then some words"), None);
            assert_eq!(bare_id("Name:'Thing' https://tracker.example.org/t/9"), None);
        }

        /// A full announce line ends in the same URL, so the bare-id branch must
        /// not claim it -- the announce regex has to keep first refusal, or the
        /// release name would be thrown away.
        #[test]
        fn a_full_announce_line_is_not_a_bare_link() {
            let msg = format!("{ANNOUNCE}");
            assert_eq!(bare_id(&msg), None);
        }

        #[test]
        fn something_that_is_neither_is_rejected() {
            assert_eq!(bare_id("please add the new one"), None);
            assert_eq!(bare_id(""), None);
        }

        fn entry(name: &str, size: i64, done: i64) -> TorrentInfo {
            TorrentInfo {
                name: name.to_string(),
                size_bytes: size,
                completed_bytes: done,
                ratio_permille: 1500,
            }
        }

        #[test]
        fn an_empty_list_says_so_rather_than_returning_nothing() {
            assert_eq!(format_download_list(&[], ReplyLimits::irc(40)), vec!["No torrents.".to_string()]);
        }

        /// The point of the change: one line each, so nothing is lost to a
        /// single message's length limit.
        #[test]
        fn every_torrent_gets_its_own_line() {
            let list = [entry("Alpha", 1024, 1024), entry("Beta", 2048, 512)];
            let out = format_download_list(&list, ReplyLimits::irc(40));

            assert_eq!(out.len(), 3, "header plus one line each: {out:?}");
            assert_eq!(out[0], "2 torrents (1 complete):");
            assert!(out[1].starts_with("[0] Alpha — 1.00KiB, done, ratio 1.50"), "{}", out[1]);
            assert!(out[2].starts_with("[1] Beta — 2.00KiB, 25%, ratio 1.50"), "{}", out[2]);
        }

        #[test]
        fn the_count_is_singular_for_one() {
            assert!(format_download_list(&[entry("Alpha", 1, 0)], ReplyLimits::irc(40))[0].starts_with("1 torrent ("));
        }

        /// A torrent with no size yet -- a magnet whose metadata has not arrived
        /// -- must not divide by zero.
        #[test]
        fn a_torrent_with_no_size_reports_zero_rather_than_panicking() {
            let out = format_download_list(&[entry("Pending", 0, 0)], ReplyLimits::irc(40));
            assert!(out[1].contains("0%"), "{}", out[1]);
        }

        /// Long listings are capped so the reply cannot become a flood, and the
        /// remainder is counted rather than silently dropped.
        #[test]
        fn a_very_long_list_is_capped_and_says_what_it_dropped() {
            let list: Vec<_> =
                (0..500).map(|i| entry(&format!("Release {i:03}"), 1024, 1024)).collect();
            let out = format_download_list(&list, ReplyLimits::irc(40));

            assert!(out.len() <= 41, "{} lines", out.len());
            assert_eq!(out[0], "500 torrents (500 complete):");
            assert!(out.last().unwrap().contains("more (not shown)"), "{:?}", out.last());
        }

        /// A name longer than one IRC message must be cut, and not
        /// mid-character: names are arbitrary UTF-8 from the torrent, and
        /// slicing on a byte offset would panic.
        #[test]
        fn an_overlong_multibyte_name_is_cut_on_a_char_boundary() {
            let out = format_download_list(&[entry(&"é".repeat(500), 1024, 1024)], ReplyLimits::irc(40));

            assert!(out[1].contains('…'), "{}", out[1]);
            // Still valid UTF-8, and inside one IRC message.
            assert!(out[1].len() <= LINE_BUDGET + 4, "{}", out[1].len());
        }

        /// Both of those cuts are IRC's, and neither should reach a transport
        /// that takes the reply whole -- Telegram and Slack fit a 500-torrent
        /// listing across a handful of messages, and answering `lt!` there with
        /// twelve rows and "… and 488 more" is a limit invented for nothing.
        #[test]
        fn an_unconstrained_transport_gets_the_whole_listing_untrimmed() {
            let list: Vec<TorrentInfo> = (0..500).map(|i| entry(&format!("R{i}"), 1, 1)).collect();
            let out = format_download_list(&list, ReplyLimits::unconstrained());

            assert_eq!(out.len(), 501, "header plus every torrent");
            assert!(
                !out.iter().any(|l| l.contains("not shown")),
                "nothing should be dropped"
            );

            // And a long name arrives intact rather than cut to IRC's 380 bytes.
            let long = format_download_list(
                &[entry(&"é".repeat(500), 1024, 1024)],
                ReplyLimits::unconstrained(),
            );
            assert!(!long[1].contains('…'), "{}", long[1]);
            assert!(long[1].len() > LINE_BUDGET, "{}", long[1].len());
        }

        /// A regex argument is full of parentheses -- groups, alternations,
        /// inline flags -- and they must not truncate the parameter at the first
        /// `)`. The `.*` is greedy, so it backtracks to the *last* `)` in the
        /// message, which is the real delimiter whenever the parameter ends the
        /// line. These are the shapes that would break a lazy or
        /// first-close-wins parse.
        #[test]
        fn parentheses_inside_the_parameter_survive() {
            for pattern in [
                "Some.(Release|Thing).*1080p",
                "(?i)nordic",
                r"^(?:A|B)\d{2}$",
                "outer(inner(deepest))end",
                r"escaped\)paren",
                "(?i)(Show.One|Show.Two).*(1080p|2160p).*(WEB|BluRay)",
            ] {
                let msg = format!("cmd:addtowatchlist params:({pattern})");
                let (command, params) = parse(&msg).expect("should parse");
                assert_eq!(command, "addtowatchlist");
                assert_eq!(params, pattern, "parameter was truncated: {msg}");
            }
        }

        /// Greedy matching backtracks to the *last* `)` in the message, so
        /// ordinary trailing text is simply left outside the parameter.
        #[test]
        fn trailing_text_without_a_paren_is_ignored() {
            assert_eq!(parse("cmd:addtowatchlist params:(foo) please").unwrap().1, "foo");
        }

        /// The limit of that rule, recorded rather than left to be discovered:
        /// trailing text that itself contains `)` moves the delimiter, so it is
        /// absorbed into the parameter. Not silent -- the result no longer
        /// compiles as a regex and `addtowatchlist` now reports that instead of
        /// logging it and answering "added".
        #[test]
        fn trailing_text_containing_a_paren_moves_the_delimiter() {
            let (_, params) = parse("cmd:addtowatchlist params:(foo) and (bar)").unwrap();
            assert_eq!(params, "foo) and (bar");
            assert!(Regex::new(&params).is_err(), "so the caller reports it as invalid");
        }

        /// An empty parameter must not reach add_dl_regex, where an empty regex
        /// compiles happily and then matches every announce line.
        #[test]
        fn an_empty_parameter_is_distinguishable() {
            assert_eq!(parse("cmd:addtowatchlist params:()").unwrap().1, "");
        }

        #[test]
        fn a_shortcut_expands_to_the_long_form() {
            assert_eq!(
                expand_shortcut("at! https://tracker.example.org/torrent/241813706").as_deref(),
                Some("cmd:addtorrent params:(https://tracker.example.org/torrent/241813706)")
            );
            assert_eq!(expand_shortcut("lt!").as_deref(), Some("cmd:torrentlist"));
            assert_eq!(expand_shortcut("rw! 2").as_deref(), Some("cmd:removewatch params:(2)"));
        }

        #[test]
        fn shortcuts_tolerate_case_and_spacing() {
            for form in ["LT!", " lt! ", "Lt!   "] {
                assert_eq!(expand_shortcut(form).as_deref(), Some("cmd:torrentlist"), "{form}");
            }
        }

        /// A regex parameter is full of parentheses, and the expansion wraps it
        /// in another pair. The canonical parser takes the last `)`, so it has
        /// to come back out exactly as typed.
        #[test]
        fn a_regex_survives_being_wrapped_by_the_expansion() {
            let pattern = "(?i)(Show.One|Show.Two).*(1080p|2160p)";
            let expanded = expand_shortcut(&format!("aw! {pattern}")).unwrap();

            let (command, params) = parse(&expanded).expect("expansion must parse");
            assert_eq!(command, "addtowatchlist");
            assert_eq!(params, pattern);
        }

        /// The important safety property. An unknown short form must fall
        /// through to the announce matcher rather than being claimed as a
        /// command -- otherwise any line with an exclamation mark near the front
        /// would stop being an announcement.
        #[test]
        fn an_unknown_short_form_is_not_a_command() {
            for msg in [
                "xyz! something",
                "Wow! what a release",
                "New Torrent Announcement: Name:'Hey! S01 1080p' uploaded by 'Anon'",
                "no exclamation here",
            ] {
                assert_eq!(expand_shortcut(msg), None, "{msg}");
            }
        }

        /// Every shortcut must name a command that actually exists, or it
        /// silently answers "not implemented".
        #[test]
        fn every_shortcut_maps_to_a_real_command() {
            let implemented = [
                "addtorrent",
                "torrentlist",
                "downloadlist",
                "watchlist",
                "listwatch",
                "help",
                "commands",
                "testnotify",
                "addtowatchlist",
                "removewatch",
                "stop",
                "cancel",
            ];
            for c in COMMANDS {
                assert!(
                    implemented.contains(&c.full),
                    "{}! maps to unknown command '{}'",
                    c.short,
                    c.full
                );
            }
        }

        /// The help must show every command in *both* spellings: the short form
        /// is the point, but the long one is what the README, the logs and every
        /// error message use.
        #[test]
        fn the_help_lists_every_command_in_both_forms() {
            let help = help_lines(ReplyLimits::irc(40)).join("\n");
            for c in COMMANDS {
                assert!(help.contains(&format!("{}!", c.short)), "help omits {}!", c.short);
                assert!(
                    help.contains(&format!("cmd:{}", c.full)),
                    "help omits cmd:{}",
                    c.full
                );
            }
        }

        /// Even the help obeys the flood cap: an operator who lowered
        /// max_reply_lines for a strict network must not be flooded by it.
        #[test]
        fn the_help_is_capped_like_any_other_listing() {
            let out = help_lines(ReplyLimits::irc(3));
            assert_eq!(out.len(), 4, "3 lines plus the footer: {out:?}");
            assert!(out.last().unwrap().contains("more (not shown)"), "{out:?}");
        }

        /// The cap is a setting, not a constant: a real network killed the bot
        /// for excess flood at around the twentieth message, so it has to be
        /// tunable downward without a rebuild.
        #[test]
        fn the_line_cap_is_honoured_exactly() {
            let list: Vec<_> = (0..100).map(|i| entry(&format!("R{i}"), 1, 0)).collect();

            for cap in [2usize, 5, 12, 40] {
                let out = format_download_list(&list, ReplyLimits::irc(cap));
                // `cap` lines, plus the one saying what was dropped.
                assert_eq!(out.len(), cap + 1, "cap {cap}: {out:?}");
                assert!(out.last().unwrap().contains("more (not shown)"), "cap {cap}");
            }
        }

        /// A listing that fits must not grow a spurious "and 0 more".
        #[test]
        fn a_listing_inside_the_cap_gets_no_footer() {
            let list: Vec<_> = (0..3).map(|i| entry(&format!("R{i}"), 1, 0)).collect();
            let out = format_download_list(&list, ReplyLimits::irc(12));

            assert_eq!(out.len(), 4, "{out:?}");
            assert!(!out.last().unwrap().contains("not shown"), "{out:?}");
        }

        fn patterns(n: usize) -> Vec<String> {
            (0..n).map(|i| format!("Some.Release.S{i:02}.*1080p.*WEB.*")).collect()
        }

        /// The point of the command: an index for every entry, because
        /// `removewatch` takes one and nothing else displays them.
        #[test]
        fn the_watchlist_is_numbered_from_zero() {
            let out = format_watchlist(&patterns(3), ReplyLimits::irc(40));
            assert_eq!(out.len(), 4, "header plus one line each: {out:?}");
            assert_eq!(out[0], "3 patterns:");
            assert!(out[1].starts_with("[0] "), "{}", out[1]);
            assert!(out[2].starts_with("[1] "), "{}", out[2]);
            assert!(out[3].starts_with("[2] "), "{}", out[3]);
        }

        /// An empty watchlist must not answer "0 patterns: " with nothing after
        /// the colon.
        #[test]
        fn an_empty_watchlist_says_how_to_add_one() {
            let out = format_watchlist(&[], ReplyLimits::irc(40));
            assert_eq!(out.len(), 1, "{out:?}");
            assert!(out[0].contains("empty"), "{}", out[0]);
            assert!(out[0].contains("addtowatchlist"), "{}", out[0]);
        }

        #[test]
        fn a_single_pattern_is_not_pluralised() {
            assert_eq!(format_watchlist(&patterns(1), ReplyLimits::irc(40))[0], "1 pattern:");
        }

        /// The indices must stay absolute when the line is cut. Renumbering the
        /// visible subset would make `removewatch <n>` delete a different pattern
        /// than the one displayed, as soon as the list outgrew one IRC line.
        #[test]
        fn truncation_does_not_renumber_the_entries() {
            let all = patterns(200);
            let out = format_watchlist(&all, ReplyLimits::irc(40));

            assert_eq!(out[0], "200 patterns:");
            assert!(out.last().unwrap().contains("more (not shown)"), "{:?}", out.last());

            // Every index printed must sit beside the pattern that actually
            // occupies that position in the full list.
            let numbered: Vec<&String> =
                out.iter().filter(|l| l.starts_with('[')).collect();
            assert!(numbered.len() > 1, "expected several entries: {out:?}");

            for line in numbered {
                let i: usize = line[1..].split(']').next().unwrap().parse().unwrap();
                assert_eq!(*line, format!("[{i}] {}", all[i]), "index {i} names the wrong pattern");
            }
        }
    }
}
