//! Telegram, as both a command source and a notification target.
//!
//! Chosen over the alternatives because it needs nothing this bot does not
//! already have: the Bot API is plain HTTPS with JSON bodies, so `reqwest`,
//! `serde_json` and `tokio` cover it with no new dependency. Receiving is *long
//! polling*, which matters more than it sounds -- there is no webhook, so no
//! inbound port, no TLS certificate and no hole in the router for a bot running
//! on a NAS behind NAT.
//!
//! The bot itself has no phone number and no account of its own; it is created
//! by messaging @BotFather.

use std::rc::Rc;
use std::time::Duration;

use anyhow::Error;
use log::{debug, error, info, warn};
use serde_json::{json, Value};

use crate::auth::{CommandRequest, Principal};
use crate::command_processor::commands::CommandProcessor;
use crate::config::config::TelegramOptions;
use crate::transports::chunk_reply;

/// Telegram accepts 4096 characters per message. The margin leaves room for the
/// ``` fences a monospace block needs, plus a little slack -- being rejected for
/// one character over is a poor trade for the last few bytes.
const MESSAGE_BUDGET: usize = 3900;

/// Messages one reply may span. Telegram rate-limits a single chat at roughly
/// one message a second, so an unbounded listing would earn a 429 rather than
/// arriving. Ten is around five hundred torrent rows -- past any real reply.
const MAX_MESSAGES: usize = 10;

/// How long the server may hold a poll open with nothing to say.
///
/// The request simply parks server-side until a message arrives or this expires,
/// so a long value is *cheaper* than a short one: fewer round trips, and a
/// command is still picked up the instant it is sent.
const POLL_SECONDS: u64 = 50;

/// Slightly longer than the poll, so a normal empty return is never mistaken for
/// a stall.
const HTTP_TIMEOUT: Duration = Duration::from_secs(POLL_SECONDS + 15);

/// Backoff after a failed poll, doubling to this ceiling.
const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);

/// Talks to the Bot API. Cloneable and `Send`, so the notification backend and
/// the command poller share one.
#[derive(Clone)]
pub struct Telegram {
    http: reqwest::Client,
    token: String,
    chat_id: i64,
    /// Always the real API in production. Overridable so the tests can point at
    /// a local stand-in and exercise the poll/dispatch/reply loop for real,
    /// rather than asserting against a mock of our own making.
    base: String,
}

impl Telegram {
    pub fn new(options: &TelegramOptions) -> Option<Self> {
        if options.token.trim().is_empty() {
            error!("[telegram] needs a `token` from @BotFather; Telegram is disabled.");
            return None;
        }
        if options.owner_id == 0 {
            error!("[telegram] needs an `owner_id`; Telegram is disabled.");
            return None;
        }

        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| error!("Could not build the Telegram HTTP client: {e}"))
            .ok()?;

        Some(Self {
            http,
            token: options.token.clone(),
            chat_id: options.owner_id,
            base: "https://api.telegram.org".to_string(),
        })
    }

    /// The token is a credential, so it must never reach a log: it lives in the
    /// URL path, which means an error carrying the URL leaks it. Every failure
    /// path here uses `without_url()` for that reason.
    fn endpoint(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.base, self.token)
    }

    async fn call(&self, method: &str, body: Value) -> Result<Value, Error> {
        let response = self
            .http
            .post(self.endpoint(method))
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::msg(format!("{method} failed: {}", e.without_url())))?;

        let status = response.status();
        let parsed: Value = response
            .json()
            .await
            .map_err(|e| Error::msg(format!("{method} returned unreadable JSON: {}", e.without_url())))?;

        if !status.is_success() || parsed.get("ok").and_then(Value::as_bool) != Some(true) {
            // Telegram puts a human-readable reason here, and it is genuinely
            // useful -- "chat not found" means the owner never messaged the bot.
            let why = parsed
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("no description")
                .to_string();
            return Err(Error::msg(format!("{method} rejected (HTTP {status}): {why}{}", hint(&why))));
        }

        Ok(parsed.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Send one message, as a monospace block so listings stay aligned.
    pub async fn send(&self, text: &str) -> Result<(), Error> {
        self.call(
            "sendMessage",
            json!({
                "chat_id": self.chat_id,
                "text": format!("```\n{}\n```", escape_code_block(text)),
                "parse_mode": "MarkdownV2",
                // A listing is full of links; previews would bury it.
                //
                // `link_preview_options`, not the older
                // `disable_web_page_preview`: that one was superseded in Bot API
                // 7.0. Telegram ignores parameters it no longer knows rather
                // than rejecting them, so the deprecated spelling would have
                // silently stopped working instead of failing loudly.
                "link_preview_options": { "is_disabled": true },
            }),
        )
        .await
        .map(|_| ())
    }

    /// Send a reply that may not fit in one message.
    pub async fn send_lines(&self, lines: &[String]) -> Result<(), Error> {
        for chunk in chunk_reply(lines, MESSAGE_BUDGET, MAX_MESSAGES) {
            self.send(&chunk).await?;
        }
        Ok(())
    }

    /// Confirm the token works and say who the bot is, so a wrong token is
    /// reported at startup rather than the first time something happens.
    pub async fn whoami(&self) -> Result<String, Error> {
        let me = self.call("getMe", json!({})).await?;
        Ok(me.get("username").and_then(Value::as_str).unwrap_or("unknown").to_string())
    }

    /// One long poll. Returns the updates and the offset to ask from next.
    async fn poll(&self, offset: i64) -> Result<(Vec<Update>, i64), Error> {
        let result = self
            .call(
                "getUpdates",
                json!({
                    "offset": offset,
                    "timeout": POLL_SECONDS,
                    // Only messages; ignore edits, callbacks and the rest.
                    "allowed_updates": ["message"],
                }),
            )
            .await?;

        let mut next = offset;
        let mut updates = Vec::new();

        for raw in result.as_array().cloned().unwrap_or_default() {
            let id = raw.get("update_id").and_then(Value::as_i64).unwrap_or(0);
            // Acknowledge by asking for id+1 next time, even for updates we do
            // not care about -- otherwise the server replays them forever.
            next = next.max(id + 1);

            let Some(message) = raw.get("message") else { continue };
            let (Some(from), Some(text)) = (
                message.get("from").and_then(|f| f.get("id")).and_then(Value::as_i64),
                message.get("text").and_then(Value::as_str),
            ) else {
                continue;
            };

            updates.push(Update { from, text: text.to_string() });
        }

        Ok((updates, next))
    }
}

struct Update {
    from: i64,
    text: String,
}

/// Attach the fix to the failures whose description does not imply one.
///
/// Telegram's wording is usually good enough on its own; these two are not.
fn hint(description: &str) -> &'static str {
    let lower = description.to_ascii_lowercase();

    if lower.contains("chat not found") {
        // The setup trap: Telegram forbids a bot opening a conversation, so the
        // owner has to message it first. Nothing in the message says that.
        " -- message the bot yourself once first; a bot cannot open the chat"
    } else if lower.contains("terminated by other getupdates")
        || lower.contains("can't use getupdates method while webhook is active")
    {
        // Two pollers on one token, which in practice means an old container
        // still running. They take turns stealing each other's updates, so
        // commands work intermittently and nothing looks broken.
        " -- another instance is polling this bot, or a webhook is set: only one may"
    } else {
        ""
    }
}

/// MarkdownV2 treats a backtick as markup even inside a fence, so a torrent name
/// containing one would break out of the block and produce a parse error --
/// which Telegram rejects the whole message for. Backslash is escaped for the
/// same reason.
fn escape_code_block(text: &str) -> String {
    text.replace('\\', "\\\\").replace('`', "\\`")
}

/// Receive commands from Telegram until the process ends.
///
/// **Never returns.** It runs as an arm of the `select!` in `Irc2Torrent::start`,
/// and a completed arm cancels the others -- the same trap that would have shut
/// the bot down when `notify::poll` returned early for an unconfigured install.
/// With no Telegram configured this parks forever instead.
///
/// It also cannot be `tokio::spawn`ed: `CommandProcessor` is behind `Rc` and is
/// not `Send`. Running on the shared task is fine, because every wait here is an
/// await that yields to the IRC loop.
pub async fn receive_commands(cp: Rc<CommandProcessor>, telegram: Option<Telegram>) {
    let Some(telegram) = telegram else {
        std::future::pending::<()>().await;
        return;
    };

    // Start from 0: Telegram then replays anything queued while the bot was
    // down, which is what you want for a command you sent during a restart.
    let mut offset: i64 = 0;
    let mut backoff = RETRY_MIN;
    // Confirmed on the first poll that works, not once up front.
    //
    // A container commonly starts its bot before the network is wired up, so an
    // eager check there fails for a reason that has nothing to do with the
    // token -- and printed "Could not confirm the Telegram token", which reads
    // exactly like the token being wrong. Worse, it never tried again, so the
    // reassuring line never appeared even once Telegram was reachable.
    let mut confirmed = false;
    let mut failures: u32 = 0;

    loop {
        match telegram.poll(offset).await {
            Ok((updates, next)) => {
                if failures > 0 {
                    info!("Telegram is reachable again (after {failures} failed polls).");
                    failures = 0;
                }
                backoff = RETRY_MIN;
                offset = next;

                if !confirmed {
                    confirmed = true;
                    match telegram.whoami().await {
                        Ok(username) => info!("Telegram commands enabled as @{username}."),
                        Err(e) => warn!("Telegram is reachable but getMe failed: {e}"),
                    }
                }

                for update in updates {
                    handle(&cp, &telegram, update).await;
                }
            }
            Err(e) => {
                // Once at ERROR, then quietly. A network outage lasts minutes
                // and the backoff caps at a minute, so logging every attempt
                // buries whatever else is happening -- and the first line
                // already said everything the tenth would.
                if failures == 0 {
                    error!("Telegram poll failed: {e}");
                } else {
                    debug!("Telegram poll still failing ({failures} so far): {e}");
                }
                failures += 1;

                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RETRY_MAX);
            }
        }
    }
}

async fn handle(cp: &Rc<CommandProcessor>, telegram: &Telegram, update: Update) {
    if !cp.is_command(&update.text) {
        debug!("Telegram message from {} is not a command; ignoring.", update.from);
        return;
    }

    let request = CommandRequest {
        principal: Principal::Telegram { user_id: update.from },
        // Telegram is configured against one chat, and authorization compares
        // the sender's ID, so there is no channel/private distinction to make.
        direct: true,
        text: update.text.clone(),
    };

    let reply = match cp.process_command(&request).await {
        Ok(lines) => lines,
        Err(e) => vec![e.to_string()],
    };

    if let Err(e) = telegram.send_lines(&reply).await {
        error!("Could not reply over Telegram: {e}");
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// A backtick in a torrent name would otherwise close the code fence, and
    /// Telegram rejects the whole message as a MarkdownV2 parse error -- so the
    /// reply vanishes rather than arriving slightly wrong.
    #[test]
    fn code_block_markup_is_escaped() {
        assert_eq!(escape_code_block("Some`Release"), "Some\\`Release");
        assert_eq!(escape_code_block(r"back\slash"), r"back\\slash");
        assert_eq!(escape_code_block("ordinary name"), "ordinary name");
    }

    fn client_against(base: &str) -> Telegram {
        Telegram {
            http: reqwest::Client::builder().timeout(Duration::from_secs(5)).build().unwrap(),
            token: "123:test".into(),
            chat_id: 4242,
            base: base.to_string(),
        }
    }

    /// The whole loop against a real socket: poll, parse, reply.
    ///
    /// This is the part unit tests cannot reach -- that the request shape is
    /// what Telegram expects, that `offset` advances so an update is not
    /// replayed forever, and that the reply actually goes to `sendMessage`.
    #[tokio::test]
    async fn the_poller_advances_its_offset_and_posts_a_reply() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Answers two getUpdates calls (one with an update, one empty) and one
        // sendMessage, recording every request line and body.
        let server = tokio::spawn(async move {
            let mut seen: Vec<String> = Vec::new();
            for reply in [
                r#"{"ok":true,"result":[{"update_id":77,"message":{"from":{"id":4242},"text":"lt!"}}]}"#,
                r#"{"ok":true,"result":[]}"#,
                r#"{"ok":true,"result":{}}"#,
            ] {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap();
                seen.push(String::from_utf8_lossy(&buf[..n]).to_string());

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    reply.len(),
                    reply
                );
                sock.write_all(response.as_bytes()).await.unwrap();
                sock.flush().await.unwrap();
            }
            seen
        });

        let telegram = client_against(&format!("http://127.0.0.1:{port}"));

        // First poll yields the update; the offset it returns must be past it.
        let (updates, next) = telegram.poll(0).await.unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].from, 4242);
        assert_eq!(updates[0].text, "lt!");
        assert_eq!(next, 78, "offset must advance past the update, or it repeats forever");

        // Second poll: nothing new, offset unchanged.
        let (empty, still) = telegram.poll(next).await.unwrap();
        assert!(empty.is_empty());
        assert_eq!(still, 78);

        telegram.send("2 torrents (1 complete):").await.unwrap();

        let seen = server.await.unwrap();
        assert!(seen[0].starts_with("POST /bot123:test/getUpdates"), "{}", &seen[0][..40]);
        assert!(seen[0].contains("\"offset\":0"), "{}", seen[0]);
        assert!(seen[1].contains("\"offset\":78"), "second poll should ask past it: {}", seen[1]);
        assert!(seen[2].starts_with("POST /bot123:test/sendMessage"), "{}", &seen[2][..40]);
        assert!(seen[2].contains("2 torrents"), "{}", seen[2]);
    }

    /// Telegram signals failure in the body with `ok: false`, not only by status
    /// code, so a 200 carrying a rejection must still be an error here.
    #[tokio::test]
    async fn an_ok_false_body_is_treated_as_a_failure() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let _ = sock.read(&mut vec![0u8; 4096]).await;
            let body = r#"{"ok":false,"description":"chat not found"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(response.as_bytes()).await.unwrap();
        });

        let err = client_against(&format!("http://127.0.0.1:{port}"))
            .send("anything")
            .await
            .expect_err("ok:false must not be treated as success");

        // The reason is the useful part: "chat not found" means the owner never
        // messaged the bot, which no status code would have told us.
        assert!(err.to_string().contains("chat not found"), "{err}");
    }

    /// Absent config must not enable the transport, and must not panic.
    #[test]
    fn an_incomplete_section_disables_the_transport() {
        use crate::config::config::{EventFilter, TelegramOptions};

        let base = TelegramOptions {
            token: "123:abc".into(),
            owner_id: 42,
            commands: true,
            notifications: true,
            events: EventFilter::default(),
        };

        assert!(Telegram::new(&base).is_some());
        assert!(Telegram::new(&TelegramOptions { token: "  ".into(), ..base.clone() }).is_none());
        assert!(Telegram::new(&TelegramOptions { owner_id: 0, ..base.clone() }).is_none());
    }
}
