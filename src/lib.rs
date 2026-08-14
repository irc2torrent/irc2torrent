use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crate::clients::{TorrentClientsEnum, Unrecoverable};
use crate::clients::flood::Flood;
use crate::clients::qbittorrent::QBittorrent;
use crate::clients::rtorrent::rTorrent;
use log::error;
use crate::command_processor::commands::CommandProcessor;
use crate::config::config::{Config, SecurityMode, TorrentClientOption};
use crate::irc_processor::irc::IrcProcessor;
use crate::platforms::TorrentPlatform;
use crate::platforms::http::HttpTracker;
use crate::torrent_processor::torrent::TorrentProcessor;
use tokio::select;
use tokio::time::{Duration, Instant, interval_at};

mod announce;
mod template;
mod irc_processor;
mod command_processor;
mod torrent_processor;
mod config;
mod clients;
mod platforms;
mod auth;
mod transports;
pub mod supervisor;
pub mod notify;
pub mod logging;

static IRC_CONFIG_FILE: &str = "irc.toml";
static OPTIONS_CONFIG_FILE: &str = "options.toml";
const PERIODIC_CHECK_INTERVAL: u64 = 60;

async fn periodic_check(irc: Rc<RefCell<IrcProcessor>>, nick: &str) {
    let start_time = Instant::now();
    let mut interval = interval_at(start_time, Duration::from_secs(PERIODIC_CHECK_INTERVAL));
    loop {
        irc.borrow().update_user_status(nick);
        interval.tick().await;
    }
}

/// What the greeting says the bot is wired to.
///
/// Worth naming rather than just "started": the usual reason for reading it is
/// that you just changed the config, and this is what confirms the change was
/// picked up -- a `[slack]` typo shows up here as Slack simply not being listed.
fn describe_integrations(cfg: &Config) -> String {
    let notifications = cfg.get_notifications();
    let mut parts: Vec<&str> = Vec::new();

    if cfg.is_commands_enabled() {
        parts.push("commands");
    }
    if notifications.email.is_some() {
        parts.push("email");
    }
    if notifications.ntfy.is_some() {
        parts.push("ntfy");
    }
    if notifications.irc.is_some() {
        parts.push("irc");
    }
    if cfg.get_telegram().is_some() {
        parts.push("telegram");
    }
    if cfg.get_slack().is_some() {
        parts.push("slack");
    }

    if parts.is_empty() {
        "no integrations configured".to_string()
    } else {
        parts.join(", ")
    }
}

pub struct Irc2Torrent {
    config: Rc<RefCell<Config>>,
    torrent_processor: Rc<TorrentProcessor>,
    command_processor: Box<Rc<CommandProcessor>>,
    irc_processor: Rc<RefCell<IrcProcessor>>,
    notification_options: crate::config::config::NotificationOptions,
    notifier: notify::Notifier,
    /// Present only when Telegram is configured to accept commands.
    telegram_commands: Option<transports::telegram::Telegram>,
    /// Likewise for Slack. Absent here means no socket is opened at all --
    /// notifications still work, because posting needs no socket.
    slack_commands: Option<transports::slack::Slack>,
}
const CLIENT_MAX_RETRY: u8 = 10;
impl Irc2Torrent {
    pub async fn new() -> Self {
        let torrent = pub_sub::PubSub::new();
        let torrent_ch = torrent.clone();
        let commands = pub_sub::PubSub::new();
        let command_ch = commands.clone();
        let irc = pub_sub::PubSub::new();
        let irc_ch = irc.clone();
        // Printed with Display, not the anyhow Debug an `.unwrap()` would give:
        // the "no tracker configured" error is a multi-line message written to
        // be read, and a first run always hits it.
        let mut cfg = Config::new().await.unwrap_or_else(|e| panic!("{e}"));
        // Naming the config in the panic, rather than the bare unwrap this used
        // to be: an empty [[clients]] array is a normal thing to end up with
        // while switching clients, and it deserves better than a backtrace.
        let mut configured = cfg
            .get_torrent_client()
            .unwrap_or_else(|e| panic!("{e}"));
        let mut torrent_client = Irc2Torrent::get_torrent_client(&mut configured).await;
        let platform = cfg.get_torrent_platform();
        // Already validated by LoadedOptions::from_data, so this cannot fail in
        // practice -- but it is the same parse, so report it the same way rather
        // than unwrapping.
        let mut torrent_platform = HttpTracker::new(&platform.label, &platform.options)
            .unwrap_or_else(|e| panic!("{e}"));
        let notification_options = cfg.get_notifications();
        let config = Rc::new(RefCell::new(cfg));

        // The IRC sender exists only once connected, and is replaced on every
        // reconnect, so the backend gets a slot that IrcProcessor republishes
        // into rather than a sender captured here and stale by the first drop.
        let irc_sender_slot: notify::IrcSenderSlot = Default::default();
        let irc_owner = match config.borrow().get_security_mode() {
            SecurityMode::IrcUserName(nick) => Some((irc_sender_slot.clone(), nick)),
            // Password mode identifies a message, not a person, so there is
            // nobody to send an unprompted private message to.
            SecurityMode::Password(_) => None,
        };
        // One Telegram client for both roles. Building it twice would let the
        // notification target and the command source drift apart, and it is the
        // same token either way.
        let telegram_options = config.borrow().get_telegram();
        let telegram = telegram_options.as_ref().and_then(transports::telegram::Telegram::new);

        let telegram_notify = match (&telegram, &telegram_options) {
            (Some(client), Some(o)) if o.notifications => Some((client.clone(), o.events.clone())),
            _ => None,
        };
        let telegram_commands = match (&telegram, &telegram_options) {
            (Some(client), Some(o)) if o.commands => Some(client.clone()),
            _ => None,
        };

        // Slack, the same way: one client, both roles.
        let slack_options = config.borrow().get_slack();
        let slack = slack_options.as_ref().and_then(transports::slack::Slack::new);

        let slack_notify = match (&slack, &slack_options) {
            (Some(client), Some(o)) if o.notifications => Some((client.clone(), o.events.clone())),
            _ => None,
        };
        let slack_commands = match (&slack, &slack_options) {
            (Some(client), Some(o)) if o.commands => Some(client.clone()),
            _ => None,
        };

        let shared_notifications = config.borrow().shared_notifications();
        let notifier = notify::start(
            notification_options.clone(),
            shared_notifications,
            irc_owner,
            telegram_notify,
            slack_notify,
        );
        config.borrow().set_notifier(notifier.clone());

        // Greet every configured target. Buffered like any other event rather
        // than sent at once: at this point the client is up but the network may
        // not be -- a container starts its bot before Docker has finished
        // wiring it -- and an immediate send would simply fail and be lost. It
        // goes out with the first digest instead.
        notifier.send(notify::Event::Started {
            version: env!("CARGO_PKG_VERSION"),
            integrations: describe_integrations(&config.borrow()),
        });

        let torrent_processor = Rc::new(
            TorrentProcessor::new(config.clone(), torrent_ch, vec![commands.clone().subscribe(), irc.clone().subscribe()], torrent_client, torrent_platform, notifier.clone()));
        let command_processor = Rc::new(
            CommandProcessor::new(config.clone(), torrent_processor.clone(), command_ch, vec![torrent.clone().subscribe(), irc.clone().subscribe()], notifier.clone(), irc_sender_slot.clone()));
        let irc_processor = Rc::new(RefCell::new(
            IrcProcessor::new(config.clone(), torrent_processor.clone(), command_processor.clone(), irc_ch, vec![torrent.clone().subscribe(), commands.clone().subscribe()], notifier.clone(), irc_sender_slot)));
        /*if let SecurityMode::IrcUserName(nick) = config.borrow().get_security_mode() {
            select! {
                _ = periodic_check(irc_processor.clone(), &nick) => {}
            }
        }*/
        Self { config, torrent_processor, command_processor: Box::new(command_processor), irc_processor: irc_processor, notification_options, notifier, telegram_commands, slack_commands }
    }

    pub async fn start(&mut self) {
        // The poller runs beside the IRC loop rather than inside it: it awaits
        // RPC calls that can take seconds, and anything slow on the IRC task
        // stops the client answering server PINGs.
        let poller = notify::poll(
            self.torrent_processor.clone(),
            self.notification_options.clone(),
            self.config.borrow().shared_notifications(),
            self.notifier.clone(),
        );
        // Telegram receives on the shared task rather than a spawned one:
        // CommandProcessor is behind Rc and is not Send. That is fine here --
        // every wait in the poller is an await that yields to the IRC loop.
        let telegram = transports::telegram::receive_commands(
            (*self.command_processor).clone(),
            self.telegram_commands.clone(),
        );
        let slack = transports::slack::receive_commands(
            (*self.command_processor).clone(),
            self.slack_commands.clone(),
        );

        // The borrow is bound rather than inlined into the select arm: a
        // temporary RefMut there is dropped at the end of the statement while
        // the future still holds it.
        let mut irc = self.irc_processor.borrow_mut();

        // Every arm here must run forever. A completed arm cancels the others,
        // which is how an early `return` in the notification poller would once
        // have shut the whole bot down on any install without notifications --
        // both `poll` and `receive_commands` park on `pending()` instead.
        select! {
            _ = irc.start_listening() => {}
            _ = poller => {}
            _ = telegram => {}
            _ = slack => {}
        }
    }

    /// Connect to the configured client, retrying while it is merely not up yet.
    ///
    /// Two things this used to get wrong:
    ///
    ///   * The error was **discarded** (`if let Ok(c) = client`, no else), so
    ///     ten failed attempts produced no log line at all and the final panic
    ///     named no cause.
    ///   * It retried everything, including a rejected password. qBittorrent
    ///     bans an IP for an hour after five failed logins, and ten attempts in
    ///     thirty seconds clears that comfortably -- so a typo in `options.toml`
    ///     became an hour of lockout. `Unrecoverable` now stops on the first.
    async fn get_torrent_client(clients: &mut TorrentClientOption) -> TorrentClientsEnum {
        let mut last: Option<anyhow::Error> = None;

        for attempt in 1..=CLIENT_MAX_RETRY {
            if attempt > 1 {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }

            let client = match clients {
                TorrentClientOption::rTorrent(ref mut c) => {
                    rTorrent::new(c.xmlrpc_url.clone()).await.map(TorrentClientsEnum::Rtorrent)
                }
                TorrentClientOption::Flood(ref mut c) => {
                    Flood::new(
                        c.username.clone(),
                        c.password.clone(),
                        c.url.clone(),
                        c.destination.clone(),
                    )
                        .await
                        .map(TorrentClientsEnum::Flood)
                }
                TorrentClientOption::QBittorrent(ref mut c) => {
                    QBittorrent::new(c)
                        .await
                        .map(TorrentClientsEnum::QBittorrent)
                }
            };

            match client {
                Ok(c) => return c,
                Err(e) if e.downcast_ref::<Unrecoverable>().is_some() => {
                    panic!("The torrent client configuration is wrong: {e}");
                }
                Err(e) => {
                    error!("Could not reach the torrent client ({attempt}/{CLIENT_MAX_RETRY}): {e}");
                    last = Some(e);
                }
            }
        }

        panic!(
            "Failed to connect to the torrent client after {CLIENT_MAX_RETRY} attempts: {}",
            last.map(|e| e.to_string()).unwrap_or_else(|| "no error recorded".to_string())
        );
    }
}
