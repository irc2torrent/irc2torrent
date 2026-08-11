use std::cell::RefCell;
use std::rc::Rc;

use regex::Regex;
use subtle::ConstantTimeEq;

use crate::auth::AuthResult::{NotAuthorized, PasswordValidated, SourceValidated};
use crate::config::config::{Config, SecurityMode};

pub struct Authorization {
    config: Rc<RefCell<Config>>,
    pwd_regex: Regex,
    /// Snapshots taken at construction; see `Authorization::new`. `None` when
    /// the transport is absent or has `commands = false`.
    telegram_owner: Option<i64>,
    slack_owner: Option<String>,
}

/// Where a message came from, relative to the bot.
///
/// Note the distinction between the *sender* (who wrote it) and the *target*
/// (where it was addressed). For an IRC PRIVMSG the target is a channel name for
/// channel messages, and the bot's own nick for private messages. Conflating the
/// two is what previously made `OwnerPrivateMessage` unreachable.
pub enum SourceValidityResult {
    OwnerPrivateMessage,
    OwnerAnnounceChannel,
    OwnerPublicChannel,
    AnnounceChannel,
    InvalidSource,
}

pub enum AuthResult {
    PasswordValidated,
    SourceValidated,
    NotAuthorized,
}

/// What to do with a command-shaped IRC message, decided before any round trip.
///
/// The three outcomes differ in what they *cost*, which is the whole point:
/// only `Proceed` is allowed to spend a WHOIS, and only `Refuse` is allowed to
/// send a reply.
pub enum CommandGate {
    /// Could be authorized. Worth verifying, and worth running.
    Proceed,
    /// Cannot be authorized -- but the sender has already shown they are using
    /// the owner's nick in private, so an explanation costs one message to one
    /// person rather than one per stranger.
    Refuse(&'static str),
    /// Cannot be authorized, and the sender is nobody in particular. Say
    /// nothing: this is the path an attacker drives, and every reply on it is
    /// a message the bot was made to send by someone with no claim on it.
    Ignore,
}

pub enum MessageTypes {
    Command,
    Announcement,
    Other,
}

/// Identity of one received message, as opposed to a bare `(nick, channel)` pair
/// that invites mixing up sender and target.
pub struct MessageOrigin<'a> {
    /// Who sent the message.
    pub sender: &'a str,
    /// Where it was addressed: a channel, or our own nick for a private message.
    pub target: &'a str,
    /// The nick this bot is currently using.
    pub our_nick: &'a str,
}

impl<'a> MessageOrigin<'a> {
    pub fn new(sender: &'a str, target: &'a str, our_nick: &'a str) -> Self {
        Self { sender, target, our_nick }
    }

    fn is_private_message(&self) -> bool {
        self.target.eq_ignore_ascii_case(self.our_nick)
    }
}

/// Who is asking, in whatever terms their transport can prove.
///
/// Commands arrive over several transports now, and they do not agree on what
/// identity *is*. An IRC nick is a claim -- it can be taken by someone else the
/// moment you disconnect, which is why the IRC path pays for a WHOIS. A Telegram
/// or Slack user ID is issued by the platform and travels with the message; it
/// cannot be borrowed, so there is nothing to verify.
///
/// Keeping that difference in the type stops the weaker case quietly setting the
/// standard for the stronger ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    Irc { nick: String },
    Telegram { user_id: i64 },
    /// Slack IDs are strings, not numbers: `U01234567`.
    Slack { user_id: String },
}

/// One command, and enough about its sender to decide whether to run it.
///
/// Replaces `MessageOrigin` on the command path only. Announcements still arrive
/// exclusively over IRC and keep using `MessageOrigin` -- that authorization was
/// subtle enough to get wrong once already, and it gains nothing from being
/// generalised for transports that will never carry an announce line.
pub struct CommandRequest {
    pub principal: Principal,
    /// A private conversation rather than a shared channel.
    pub direct: bool,
    pub text: String,
}

impl Authorization {
    pub fn new(config: Rc<RefCell<Config>>) -> Self {
        // Read once, deliberately. The rest of `[telegram]` / `[slack]` -- the
        // token, the client, the chat replies go to -- is consumed at startup,
        // so honouring a live edit to `owner_id` alone would authorize a new
        // person while every answer still went to the old conversation. The
        // whole table is restart-only, and the reload warns about it.
        let telegram_owner = config.borrow().telegram_owner();
        let slack_owner = config.borrow().slack_owner();

        Self {
            config,
            pwd_regex: Regex::new(r"auth:\[(?P<password>.+)]").unwrap(),
            telegram_owner,
            slack_owner,
        }
    }

    /// Whether this request may run.
    ///
    /// `commands_enabled` still gates everything: one switch turns the whole
    /// remote-control surface off, whichever transport it would arrive on.
    pub fn authorize_command(&self, request: &CommandRequest) -> AuthResult {
        if !self.config.borrow().is_commands_enabled() {
            return NotAuthorized;
        }

        match &request.principal {
            // Unchanged: the nick must be the configured owner and the message
            // private, or the password must be in the message itself.
            Principal::Irc { nick } => {
                let origin = MessageOrigin::new(nick, if request.direct { nick } else { "#channel" }, nick);
                self.check_irc_command(&origin, request.direct, &request.text)
            }

            // A platform-issued ID: compare it and nothing else. No private
            // message requirement either -- Slack is configured against one
            // channel, and a stranger posting there is rejected by the ID.
            Principal::Telegram { user_id } => match self.telegram_owner {
                Some(owner) if owner == *user_id => SourceValidated,
                _ => NotAuthorized,
            },
            Principal::Slack { user_id } => match &self.slack_owner {
                // Slack IDs are case-sensitive, so compare them exactly.
                Some(owner) if owner == user_id => SourceValidated,
                _ => NotAuthorized,
            },
        }
    }

    /// Whether a command-shaped message is worth spending anything on.
    ///
    /// The identity check is a WHOIS round trip, and it used to be spent
    /// *before* authorization ran: `IrcProcessor` routes on `is_command`, which
    /// is purely syntactic, so any stranger on the announce channel could make
    /// the bot query the server -- on a default install too, where
    /// `commands_enabled` is false and the command surface is meant to be off
    /// entirely. The refusal that came back then named the owner's services
    /// account, handing a piece of the config to anyone who typed `h!`.
    ///
    /// This settles the question from what is already known, for free.
    ///
    /// It must agree with `check_irc_command`, which makes the real decision
    /// once the check completes: this may only decline what would be refused
    /// there anyway. `the_gate_never_declines_what_the_real_check_allows` pins
    /// that down.
    pub fn gate_irc_command(&self, nick: &str, direct: bool) -> CommandGate {
        let enabled = self.config.borrow().is_commands_enabled();
        let mode = self.config.borrow().get_security_mode();

        match mode {
            // The credential *is* the sender, so the sender settles it.
            SecurityMode::IrcUserName(_) => {
                if !direct || !self.is_owner(nick) {
                    CommandGate::Ignore
                } else if !enabled {
                    CommandGate::Refuse(
                        "Commands are disabled on this bot. Set \
                         command_options.commands_enabled = true to use them.",
                    )
                } else {
                    CommandGate::Proceed
                }
            }
            // The credential is inside the message, so nothing about the sender
            // rules it out -- and nothing about them identifies the owner
            // either, which is why this branch never explains itself to anyone.
            SecurityMode::Password(_) => {
                if enabled {
                    CommandGate::Proceed
                } else {
                    CommandGate::Ignore
                }
            }
        }
    }

    /// The IRC half of `authorize_command`, kept in its original shape.
    fn check_irc_command(
        &self,
        origin: &MessageOrigin<'_>,
        direct: bool,
        message: &str,
    ) -> AuthResult {
        let mode = self.config.borrow().get_security_mode();
        match mode {
            SecurityMode::IrcUserName(_) => {
                if direct && self.is_owner(origin.sender) {
                    return SourceValidated;
                }
            }
            SecurityMode::Password(ref expected) => {
                if let Some(caps) = self.pwd_regex.captures(message) {
                    let supplied = &caps["password"];
                    // Constant-time compare: ordinary String equality returns as
                    // soon as two bytes differ. Not a practical attack over IRC,
                    // but it costs nothing to remove the signal.
                    if bool::from(supplied.as_bytes().ct_eq(expected.as_bytes())) {
                        return PasswordValidated;
                    }
                }
            }
        }
        NotAuthorized
    }

    pub fn authenticate(
        &self,
        origin: &MessageOrigin<'_>,
        message: &str,
        message_type: MessageTypes,
    ) -> AuthResult {
        match message_type {
            // Commands go through `authorize_command`, which speaks in
            // `Principal` rather than IRC nicks. This function now serves the
            // announcement path only -- announcements arrive over IRC and
            // nowhere else.
            MessageTypes::Command => return NotAuthorized,
            MessageTypes::Announcement => {
                // Accept announcements on a configured channel regardless of who
                // posted them. Previously only the non-owner variant was matched,
                // so if the announcer happened to be the configured owner every
                // announcement was silently dropped.
                match self.validate_source(origin) {
                    SourceValidityResult::AnnounceChannel
                    | SourceValidityResult::OwnerAnnounceChannel => return SourceValidated,

                    // The owner may also hand the bot an announce line by
                    // private message, to queue something by hand.
                    //
                    // Gated on commands_enabled rather than always on. Identity
                    // here is only an IRC nick, which is spoofable when the
                    // owner is not connected and the network does not enforce
                    // registration -- so this rides on the same switch as the
                    // rest of the remote-control surface instead of quietly
                    // widening what a default install accepts. (In Password
                    // mode `is_owner` is never true, so this arm cannot match
                    // there at all.)
                    SourceValidityResult::OwnerPrivateMessage
                        if self.config.borrow().is_commands_enabled() =>
                    {
                        return SourceValidated
                    }

                    _ => {}
                }
            }
            MessageTypes::Other => {
                return NotAuthorized;
            }
        }
        NotAuthorized
    }

    pub fn validate_source(&self, origin: &MessageOrigin<'_>) -> SourceValidityResult {
        let is_owner = self.is_owner(origin.sender);
        let is_valid_channel = self.is_valid_channel(origin.target);

        if is_owner && origin.is_private_message() {
            SourceValidityResult::OwnerPrivateMessage
        } else if is_owner && is_valid_channel {
            SourceValidityResult::OwnerAnnounceChannel
        } else if is_owner {
            SourceValidityResult::OwnerPublicChannel
        } else if is_valid_channel {
            SourceValidityResult::AnnounceChannel
        } else {
            SourceValidityResult::InvalidSource
        }
    }

    fn is_valid_channel(&self, channel: &str) -> bool {
        let channels = self.config.borrow().get_irc_config().channels.clone();
        channels.iter().any(|c| c.eq_ignore_ascii_case(channel))
    }

    fn is_owner(&self, nick: &str) -> bool {
        if let SecurityMode::IrcUserName(valid_user) = self.config.borrow().get_security_mode() {
            // IRC nicks are case-insensitive in practice.
            return nick.eq_ignore_ascii_case(&valid_user);
        }
        false
    }
}

/// Remove credentials from a message before it reaches a log sink.
///
/// In `SecurityMode::Password` the authentication token travels inside the
/// message itself as `auth:[<password>]`, and every PRIVMSG used to be logged
/// verbatim, so the bot's own password was written to stdout and syslog on every
/// command.
pub fn redact_secrets(message: &str) -> String {
    // Built once per call rather than kept in a lazy static; logging is not hot.
    let re = Regex::new(r"auth:\[[^\]]*]").unwrap();
    re.replace_all(message, "auth:[<redacted>]").into_owned()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn private_message_is_detected_by_target_not_sender() {
        let origin = MessageOrigin::new("owner", "irc2torrent", "irc2torrent");
        assert!(origin.is_private_message());

        // A channel message is not a PM even when the owner sent it.
        let origin = MessageOrigin::new("owner", "#announces", "irc2torrent");
        assert!(!origin.is_private_message());

        // The old bug: comparing sender to target never matches for a real PM.
        let origin = MessageOrigin::new("owner", "irc2torrent", "irc2torrent");
        assert_ne!(origin.sender, origin.target);
        assert!(origin.is_private_message());
    }

    /// The owner's private message is accepted as an announcement only while
    /// commands_enabled is set, so a default install (where it is false) does
    /// not silently start taking download instructions over PM.
    #[test]
    fn owner_private_message_is_an_announcement_only_when_commands_are_enabled() {
        use crate::config::config::{Config, SecurityMode};
        use std::cell::RefCell;
        use std::rc::Rc;

        let build = |enabled: bool| {
            let mut cfg = Config::default_for_test();
            cfg.set_for_test(SecurityMode::IrcUserName("owner".into()), enabled);
            Authorization::new(Rc::new(RefCell::new(cfg)))
        };

        let origin = MessageOrigin::new("owner", "bot", "bot");

        assert!(matches!(
            build(true).authenticate(&origin, "", MessageTypes::Announcement),
            SourceValidated
        ));
        assert!(matches!(
            build(false).authenticate(&origin, "", MessageTypes::Announcement),
            NotAuthorized
        ));
    }

    /// A stranger's private message is never an announcement, whatever the
    /// commands setting: only the configured owner qualifies.
    #[test]
    fn a_stranger_private_message_is_never_an_announcement() {
        use crate::config::config::{Config, SecurityMode};
        use std::cell::RefCell;
        use std::rc::Rc;

        let mut cfg = Config::default_for_test();
        cfg.set_for_test(SecurityMode::IrcUserName("owner".into()), true);
        let auth = Authorization::new(Rc::new(RefCell::new(cfg)));

        let origin = MessageOrigin::new("someone-else", "bot", "bot");
        assert!(matches!(
            auth.authenticate(&origin, "", MessageTypes::Announcement),
            NotAuthorized
        ));
    }

    #[test]
    fn redaction_removes_the_password() {
        let out = redact_secrets("cmd:addtorrent auth:[hunter2] params:(x)");
        assert!(!out.contains("hunter2"), "{out}");
        assert!(out.contains("auth:[<redacted>]"), "{out}");
    }

    #[test]
    fn redaction_leaves_ordinary_messages_alone() {
        let msg = "New Torrent Announcement: Name:'Something' uploaded by 'Anon'";
        assert_eq!(redact_secrets(msg), msg);
    }

    #[test]
    fn redaction_handles_several_occurrences() {
        let out = redact_secrets("auth:[a] and auth:[b]");
        assert!(!out.contains("[a]") && !out.contains("[b]"), "{out}");
    }
}

#[cfg(test)]
mod command_auth_test {
    use super::*;
    use crate::config::config::{Config, SecurityMode};

    fn authorization(build: impl FnOnce(&mut Config)) -> Authorization {
        let mut cfg = Config::default_for_test();
        cfg.set_for_test(SecurityMode::IrcUserName("owner".into()), true);
        build(&mut cfg);
        Authorization::new(Rc::new(RefCell::new(cfg)))
    }

    fn request(principal: Principal, direct: bool) -> CommandRequest {
        CommandRequest { principal, direct, text: "cmd:torrentlist".into() }
    }

    /// Editing `owner_id` and saving must NOT quietly hand control to someone
    /// new. The rest of the table -- the token, the chat replies go to -- is
    /// read once at startup, so a live-honoured owner would authorize one person
    /// while answering another. The whole table is restart-only, and the reload
    /// says so in the log.
    #[test]
    fn a_transport_owner_is_fixed_at_startup_not_reread() {
        let cfg = Rc::new(RefCell::new({
            let mut c = Config::default_for_test();
            c.set_for_test(SecurityMode::IrcUserName("owner".into()), true);
            c.set_telegram_for_test(4242, true);
            c.set_slack_for_test("U0FIRST", true);
            c
        }));
        let auth = Authorization::new(cfg.clone());

        // The config changes under us, exactly as a reload would change it.
        cfg.borrow_mut().set_telegram_for_test(9999, true);
        cfg.borrow_mut().set_slack_for_test("U0SECOND", true);

        for (principal, who) in [
            (Principal::Telegram { user_id: 9999 }, "the new telegram owner"),
            (Principal::Slack { user_id: "U0SECOND".into() }, "the new slack owner"),
        ] {
            assert!(
                matches!(auth.authorize_command(&request(principal, true)), NotAuthorized),
                "{who} must wait for a restart"
            );
        }

        // And the owner from startup still is one.
        assert!(matches!(
            auth.authorize_command(&request(Principal::Telegram { user_id: 4242 }, true)),
            SourceValidated
        ));
    }

    /// A platform-issued ID is compared exactly and nothing else is consulted --
    /// no private-message rule, no NickServ. That is the whole reason these
    /// transports are stronger than an IRC nick.
    #[test]
    fn a_telegram_owner_is_authorized_and_nobody_else_is() {
        let auth = authorization(|c| c.set_telegram_for_test(4242, true));

        assert!(matches!(
            auth.authorize_command(&request(Principal::Telegram { user_id: 4242 }, true)),
            SourceValidated
        ));
        // One digit out is a different person, not a near miss.
        for other in [4241i64, 4243, -4242, 0] {
            assert!(
                matches!(
                    auth.authorize_command(&request(Principal::Telegram { user_id: other }, true)),
                    NotAuthorized
                ),
                "{other} should be refused"
            );
        }
    }

    #[test]
    fn a_slack_owner_is_matched_case_sensitively() {
        let auth = authorization(|c| c.set_slack_for_test("U01ABCDEF", true));

        assert!(matches!(
            auth.authorize_command(&request(
                Principal::Slack { user_id: "U01ABCDEF".into() },
                true
            )),
            SourceValidated
        ));
        // Slack IDs are case-sensitive; lowercasing one is a different ID.
        for other in ["u01abcdef", "U01ABCDE", "U01ABCDEFG", ""] {
            assert!(
                matches!(
                    auth.authorize_command(&request(
                        Principal::Slack { user_id: other.into() },
                        true
                    )),
                    NotAuthorized
                ),
                "{other:?} should be refused"
            );
        }
    }

    /// `commands = false` on the transport, and `commands_enabled = false`
    /// globally, must each be enough on their own.
    #[test]
    fn either_switch_alone_closes_the_transport() {
        let per_transport = authorization(|c| c.set_telegram_for_test(4242, false));
        assert!(matches!(
            per_transport.authorize_command(&request(Principal::Telegram { user_id: 4242 }, true)),
            NotAuthorized
        ));

        let mut cfg = Config::default_for_test();
        cfg.set_for_test(SecurityMode::IrcUserName("owner".into()), false);
        cfg.set_telegram_for_test(4242, true);
        let globally_off = Authorization::new(Rc::new(RefCell::new(cfg)));
        assert!(matches!(
            globally_off.authorize_command(&request(Principal::Telegram { user_id: 4242 }, true)),
            NotAuthorized
        ));
    }

    /// An unconfigured transport must refuse rather than accept anyone.
    #[test]
    fn an_unconfigured_transport_refuses_everyone() {
        let auth = authorization(|_| {});
        assert!(matches!(
            auth.authorize_command(&request(Principal::Telegram { user_id: 1 }, true)),
            NotAuthorized
        ));
        assert!(matches!(
            auth.authorize_command(&request(Principal::Slack { user_id: "U1".into() }, true)),
            NotAuthorized
        ));
    }

    /// IRC must behave exactly as it did before the request type changed: the
    /// owner in private is allowed, a stranger is not, and neither is the owner
    /// speaking in a channel.
    #[test]
    fn irc_rules_are_unchanged() {
        let auth = authorization(|_| {});

        assert!(matches!(
            auth.authorize_command(&request(Principal::Irc { nick: "owner".into() }, true)),
            SourceValidated
        ));
        assert!(matches!(
            auth.authorize_command(&request(Principal::Irc { nick: "someone".into() }, true)),
            NotAuthorized
        ));
        assert!(matches!(
            auth.authorize_command(&request(Principal::Irc { nick: "owner".into() }, false)),
            NotAuthorized,
        ));
    }

    // -----------------------------------------------------------------------
    // The pre-authorization gate
    //
    // The identity check used to be spent before authorization ran, so anyone
    // who could type `h!` at the bot could make it query the server -- on a
    // default install too, where commands are supposed to be off entirely.
    // -----------------------------------------------------------------------

    fn gate(mode: SecurityMode, enabled: bool) -> Authorization {
        let mut cfg = Config::default_for_test();
        cfg.set_for_test(mode, enabled);
        Authorization::new(Rc::new(RefCell::new(cfg)))
    }

    /// The load-bearing property: the gate may only decline what the real check
    /// would refuse anyway. If these ever disagree, the gate is silently
    /// rejecting commands that ought to run.
    #[test]
    fn the_gate_never_declines_what_the_real_check_allows() {
        for enabled in [true, false] {
            for mode in [
                SecurityMode::IrcUserName("owner".into()),
                SecurityMode::Password("hunter2".into()),
            ] {
                for direct in [true, false] {
                    for nick in ["owner", "stranger", "OWNER"] {
                        let auth = gate(mode.clone(), enabled);
                        let text = "cmd:torrentlist auth:[hunter2]";
                        let origin = MessageOrigin::new(nick, if direct { nick } else { "#c" }, nick);

                        // What the real check decides, given the gate let it by.
                        let allowed = enabled
                            && !matches!(
                                auth.check_irc_command(&origin, direct, text),
                                NotAuthorized
                            );

                        if allowed {
                            assert!(
                                matches!(auth.gate_irc_command(nick, direct), CommandGate::Proceed),
                                "gate blocked an authorized command: {mode:?} enabled={enabled} \
                                 direct={direct} nick={nick}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// A stranger gets nothing at all: no round trip, and no reply either. The
    /// reply mattered as much as the WHOIS -- refusing a stranger by name told
    /// them the owner's services account.
    #[test]
    fn a_stranger_is_ignored_rather_than_answered() {
        let auth = gate(SecurityMode::IrcUserName("owner".into()), true);
        for (nick, direct) in [("stranger", true), ("stranger", false)] {
            assert!(
                matches!(auth.gate_irc_command(nick, direct), CommandGate::Ignore),
                "{nick} direct={direct} should be ignored in silence"
            );
        }
    }

    /// Even the owner is ignored in a channel: a channel command can never be
    /// authorized, and answering one is the loudest possible amplifier.
    #[test]
    fn the_owner_is_ignored_in_a_channel() {
        let auth = gate(SecurityMode::IrcUserName("owner".into()), true);
        assert!(matches!(auth.gate_irc_command("owner", false), CommandGate::Ignore));
    }

    /// Commands off is the default, and it must cost nothing. The owner is told
    /// why -- that reply is reachable only by whoever holds the owner's nick in
    /// private, so it is one message to one person, not one per stranger.
    #[test]
    fn commands_disabled_explains_itself_only_to_the_owner() {
        let auth = gate(SecurityMode::IrcUserName("owner".into()), false);

        let CommandGate::Refuse(why) = auth.gate_irc_command("owner", true) else {
            panic!("the owner should be told why their command did nothing");
        };
        assert!(why.contains("commands_enabled"), "{why}");

        assert!(matches!(auth.gate_irc_command("stranger", true), CommandGate::Ignore));
    }

    /// Nicks are case-insensitive here as everywhere else, or the gate would
    /// silently drop the owner's own commands after a case change.
    #[test]
    fn the_gate_matches_the_owner_case_insensitively() {
        let auth = gate(SecurityMode::IrcUserName("Owner".into()), true);
        for nick in ["owner", "OWNER", "OwNeR"] {
            assert!(
                matches!(auth.gate_irc_command(nick, true), CommandGate::Proceed),
                "{nick} is the owner"
            );
        }
    }

    /// Password mode cannot tell the owner from anyone else -- the credential is
    /// in the message -- so it must not try, and must not explain itself either.
    #[test]
    fn password_mode_lets_everyone_through_the_gate_and_explains_nothing() {
        let auth = gate(SecurityMode::Password("hunter2".into()), true);
        assert!(matches!(auth.gate_irc_command("anyone", true), CommandGate::Proceed));
        assert!(matches!(auth.gate_irc_command("anyone", false), CommandGate::Proceed));

        let off = gate(SecurityMode::Password("hunter2".into()), false);
        assert!(matches!(off.gate_irc_command("anyone", true), CommandGate::Ignore));
    }

    /// Password mode still reads the secret out of the message itself, and still
    /// ignores who sent it.
    #[test]
    fn password_mode_still_works_through_the_new_type() {
        let mut cfg = Config::default_for_test();
        cfg.set_for_test(SecurityMode::Password("hunter2".into()), true);
        let auth = Authorization::new(Rc::new(RefCell::new(cfg)));

        let mut good = request(Principal::Irc { nick: "anyone".into() }, true);
        good.text = "cmd:torrentlist auth:[hunter2]".into();
        assert!(matches!(auth.authorize_command(&good), PasswordValidated));

        let mut bad = request(Principal::Irc { nick: "anyone".into() }, true);
        bad.text = "cmd:torrentlist auth:[wrong]".into();
        assert!(matches!(auth.authorize_command(&bad), NotAuthorized));
    }
}
