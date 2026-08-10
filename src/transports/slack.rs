//! Slack, as both a command source and a notification target.
//!
//! Uses **Socket Mode**, which exists precisely for apps with no public URL: the
//! bot opens an outbound WebSocket and Slack pushes events down it. That keeps
//! the property that made Telegram attractive -- no inbound port, no TLS
//! certificate, nothing to forward on the router. Slack's other option, the
//! Events API, would need a public HTTPS endpoint.
//!
//! Two tokens, because Slack separates them by design:
//!
//!   * `app_token` (`xapp-…`) opens the socket, and *only* that.
//!   * `bot_token` (`xoxb-…`) posts messages.
//!
//! Chosen over Discord, whose Gateway would have served the same purpose, only
//! because Discord is blocked where this is deployed -- an integration nobody
//! involved can exercise is worse than none.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Error;
use futures::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use serde_json::{json, Value};
use tokio::sync::OnceCell;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::auth::{CommandRequest, Principal};
use crate::command_processor::commands::CommandProcessor;
use crate::config::config::SlackOptions;
use crate::transports::chunk_reply;

/// `chat.postMessage` accepts far more, but Slack renders long messages poorly
/// and truncates code blocks in some clients. Matching Telegram's budget keeps
/// both transports behaving the same way.
const MESSAGE_BUDGET: usize = 3900;

/// Messages one reply may span. Slack's posting limit is about one a second per
/// channel, so the same guard applies as on Telegram.
const MAX_MESSAGES: usize = 10;

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Reconnect backoff, doubling to the ceiling. Slack rotates the socket URL
/// routinely, so reconnecting is normal operation rather than an error path.
const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);

/// Where the bot reads and writes.
///
/// A channel is the obvious setup and the wrong default: every reply is visible
/// to everyone in it, and `lt!` lists your library to the room. So `channel_id`
/// is optional, and leaving it out gets a direct message with the owner instead
/// -- as private as Telegram, and no channel to invite anything to.
#[derive(Clone)]
enum Target {
    /// A channel the bot has been invited to.
    Channel(String),
    /// A DM with the owner. Slack identifies that conversation by its own `D…`
    /// id, which has to be asked for; `conversations.open` is idempotent, so
    /// this is resolved once and shared by every clone.
    Direct { owner_id: String, resolved: Arc<OnceCell<String>> },
}

/// Talks to the Slack Web API. Cloneable and `Send`, so the notification backend
/// and the command listener share one.
#[derive(Clone)]
pub struct Slack {
    http: reqwest::Client,
    app_token: String,
    bot_token: String,
    target: Target,
    /// Always the real API in production; overridable so tests can point at a
    /// local stand-in.
    base: String,
}

impl Slack {
    pub fn new(options: &SlackOptions) -> Option<Self> {
        for (what, value) in [
            ("app_token", &options.app_token),
            ("bot_token", &options.bot_token),
            ("owner_id", &options.owner_id),
        ] {
            if value.trim().is_empty() {
                error!("[slack] needs a `{what}`; Slack is disabled.");
                return None;
            }
        }

        // An empty channel_id is a half-finished edit, not a request for a DM;
        // treat it as absent rather than posting to "".
        let target = match options.channel_id.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            Some(channel) => {
                // Stated once at startup rather than warned about: it is a
                // deliberate setting, but "everyone in the channel can read
                // your torrent listing" is worth being told out loud.
                info!(
                    "[slack] posting to channel {channel}: everyone in it sees the replies. \
                     Remove channel_id for a private message instead."
                );
                Target::Channel(channel.to_string())
            }
            None => Target::Direct {
                owner_id: options.owner_id.clone(),
                resolved: Arc::new(OnceCell::new()),
            },
        };

        // Catching the swap early is worth it: the two tokens look similar and
        // failing later gives "not_allowed_token_type", which explains nothing.
        if !options.app_token.starts_with("xapp-") {
            warn!("[slack] app_token does not start with `xapp-`; it opens the socket connection.");
        }
        if !options.bot_token.starts_with("xoxb-") {
            warn!("[slack] bot_token does not start with `xoxb-`; it is the one that posts.");
        }

        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| error!("Could not build the Slack HTTP client: {e}"))
            .ok()?;

        Some(Self {
            http,
            app_token: options.app_token.clone(),
            bot_token: options.bot_token.clone(),
            target,
            base: "https://slack.com/api".to_string(),
        })
    }

    /// The conversation to post to and to accept commands from.
    ///
    /// For a DM this is a network call the first time and cached after -- shared
    /// across clones, so the notification backend and the socket listener agree
    /// on one conversation and only one of them pays for it. A failure is not
    /// cached: `get_or_try_init` leaves the cell empty, so a DM that could not
    /// be opened because the network was briefly down is retried next time.
    async fn conversation(&self) -> Result<String, Error> {
        match &self.target {
            Target::Channel(id) => Ok(id.clone()),
            Target::Direct { owner_id, resolved } => resolved
                .get_or_try_init(|| async {
                    let opened = self
                        .call("conversations.open", &self.bot_token, json!({ "users": owner_id }))
                        .await?;

                    let id = opened
                        .get("channel")
                        .and_then(|c| c.get("id"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| Error::msg("conversations.open returned no channel id"))?;

                    info!("Slack will message {owner_id} directly.");
                    Ok(id.to_string())
                })
                .await
                .cloned(),
        }
    }

    /// Slack answers HTTP 200 with `"ok": false` for most failures, so the
    /// status alone says nothing -- the `error` field is the real result.
    async fn call(&self, method: &str, token: &str, body: Value) -> Result<Value, Error> {
        let response = self
            .http
            .post(format!("{}/{method}", self.base))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            // The token rides in a header rather than the URL, but strip the URL
            // anyway so an error never carries anything unexpected.
            .map_err(|e| Error::msg(format!("{method} failed: {}", e.without_url())))?;

        let parsed: Value = response
            .json()
            .await
            .map_err(|e| Error::msg(format!("{method} returned unreadable JSON: {}", e.without_url())))?;

        if parsed.get("ok").and_then(Value::as_bool) != Some(true) {
            let why = parsed.get("error").and_then(Value::as_str).unwrap_or("unknown");
            return Err(Error::msg(format!("{method} rejected: {}", explain(why))));
        }
        Ok(parsed)
    }

    /// Post one message, as a code block so listings stay aligned.
    pub async fn send(&self, text: &str) -> Result<(), Error> {
        let channel = self.conversation().await?;
        self.call(
            "chat.postMessage",
            &self.bot_token,
            json!({
                "channel": channel,
                "text": format!("```\n{text}\n```"),
            }),
        )
        .await
        .map(|_| ())
    }

    pub async fn send_lines(&self, lines: &[String]) -> Result<(), Error> {
        for chunk in chunk_reply(lines, MESSAGE_BUDGET, MAX_MESSAGES) {
            self.send(&chunk).await?;
        }
        Ok(())
    }

    /// Ask Slack for a socket to listen on. The URL is single-use and expires
    /// quickly, so it is fetched fresh on every connect.
    async fn open_socket(&self) -> Result<String, Error> {
        let result = self.call("apps.connections.open", &self.app_token, json!({})).await?;
        result
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::msg("apps.connections.open returned no url"))
    }
}

/// Slack's error codes are terse. Translate the ones an operator will actually
/// hit into something that names the fix.
fn explain(code: &str) -> String {
    let hint = match code {
        "invalid_auth" | "not_authed" => Some("the token is wrong or has been revoked"),
        "not_allowed_token_type" => {
            Some("wrong token for this call -- app_token (xapp-) opens the socket, bot_token (xoxb-) posts")
        }
        "channel_not_found" => Some("check channel_id, and invite the bot to that channel"),
        "not_in_channel" => Some("invite the bot to the channel with /invite @yourbot"),
        "user_not_found" => Some("check owner_id -- it is a member ID (U…), not a display name"),
        "cannot_dm_bot" => Some("owner_id is another bot; it must be a person"),
        "missing_scope" => Some(
            "scopes: chat:write and connections:write always; im:write and im:history too when \
             there is no channel_id",
        ),
        _ => None,
    };
    match hint {
        Some(h) => format!("{code} ({h})"),
        None => code.to_string(),
    }
}

/// What one Socket Mode frame told us to do.
enum Frame {
    /// An event carrying a message, with the envelope that must be acked.
    Message { envelope_id: String, user: String, text: String },
    /// An envelope we do not act on but still must acknowledge, or Slack
    /// redelivers it indefinitely.
    AckOnly(String),
    /// Slack is about to close this socket; reconnect.
    Disconnect,
    /// Handshake and anything else.
    Ignore,
}

/// Classify a frame. Split out from the socket loop so it can be tested without
/// a WebSocket.
fn classify(raw: &str, channel_id: &str) -> Frame {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return Frame::Ignore;
    };

    match value.get("type").and_then(Value::as_str) {
        Some("disconnect") => return Frame::Disconnect,
        Some("hello") => return Frame::Ignore,
        Some("events_api") => {}
        // slash_commands and interactive envelopes still need acking.
        Some(_) => {
            return match value.get("envelope_id").and_then(Value::as_str) {
                Some(id) => Frame::AckOnly(id.to_string()),
                None => Frame::Ignore,
            }
        }
        None => return Frame::Ignore,
    }

    let Some(envelope_id) = value.get("envelope_id").and_then(Value::as_str) else {
        return Frame::Ignore;
    };
    let envelope_id = envelope_id.to_string();

    let Some(event) = value.get("payload").and_then(|p| p.get("event")) else {
        return Frame::AckOnly(envelope_id);
    };

    // Only plain messages in the configured channel.
    //
    // A subtype means an edit, a deletion, a join notice or -- critically -- a
    // bot_message. Without that check the bot would read its own replies and,
    // since a reply can begin with a command-looking line, answer itself.
    let is_plain_message = event.get("type").and_then(Value::as_str) == Some("message")
        && event.get("subtype").is_none()
        && event.get("bot_id").is_none();

    let in_our_channel = event.get("channel").and_then(Value::as_str) == Some(channel_id);

    if !is_plain_message || !in_our_channel {
        return Frame::AckOnly(envelope_id);
    }

    match (
        event.get("user").and_then(Value::as_str),
        event.get("text").and_then(Value::as_str),
    ) {
        (Some(user), Some(text)) => {
            Frame::Message { envelope_id, user: user.to_string(), text: text.to_string() }
        }
        _ => Frame::AckOnly(envelope_id),
    }
}

/// Receive commands from Slack until the process ends.
///
/// **Never returns**, for the same reason as the Telegram poller: it runs as an
/// arm of the `select!` in `Irc2Torrent::start`, and a completed arm cancels the
/// others. With no Slack configured it parks forever.
pub async fn receive_commands(cp: Rc<CommandProcessor>, slack: Option<Slack>) {
    let Some(slack) = slack else {
        std::future::pending::<()>().await;
        return;
    };

    let mut backoff = RETRY_MIN;
    loop {
        match run_socket(&cp, &slack).await {
            // A clean disconnect is routine: Slack rotates the URL regularly and
            // warns first. Reconnect immediately rather than backing off.
            Ok(()) => {
                debug!("Slack asked us to reconnect.");
                backoff = RETRY_MIN;
            }
            Err(e) => {
                error!("Slack socket failed: {e}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RETRY_MAX);
            }
        }
    }
}

/// One connection, from opening the socket to being told to reconnect.
async fn run_socket(cp: &Rc<CommandProcessor>, slack: &Slack) -> Result<(), Error> {
    // Resolved before the socket, not per frame: in DM mode this is a network
    // call, and doing it inside the loop would put a round trip in front of
    // every message the workspace produces.
    let conversation = slack.conversation().await?;
    let url = slack.open_socket().await?;
    let (mut socket, _) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| Error::msg(format!("could not open the Slack socket: {e}")))?;

    info!("Slack commands connected.");

    while let Some(frame) = socket.next().await {
        let frame = frame.map_err(|e| Error::msg(format!("socket error: {e}")))?;
        let WsMessage::Text(raw) = frame else {
            // Ping/pong and close frames are handled by the library.
            continue;
        };

        match classify(&raw, &conversation) {
            Frame::Disconnect => return Ok(()),
            Frame::Ignore => {}
            Frame::AckOnly(envelope_id) => {
                ack(&mut socket, &envelope_id).await?;
            }
            Frame::Message { envelope_id, user, text } => {
                // Acknowledge *before* running the command. Slack expects an ack
                // within three seconds and redelivers otherwise, and a command
                // can take far longer than that -- an unacked torrentlist would
                // be redelivered and run again.
                ack(&mut socket, &envelope_id).await?;
                handle(cp, slack, &user, &text).await;
            }
        }
    }

    Ok(())
}

async fn ack<S>(socket: &mut S, envelope_id: &str) -> Result<(), Error>
where
    S: SinkExt<WsMessage> + Unpin,
    <S as futures::Sink<WsMessage>>::Error: std::fmt::Display,
{
    socket
        .send(WsMessage::Text(json!({ "envelope_id": envelope_id }).to_string()))
        .await
        .map_err(|e| Error::msg(format!("could not acknowledge {envelope_id}: {e}")))
}

async fn handle(cp: &Rc<CommandProcessor>, slack: &Slack, user: &str, text: &str) {
    if !cp.is_command(text) {
        return;
    }

    let request = CommandRequest {
        principal: Principal::Slack { user_id: user.to_string() },
        // Slack is configured against one channel and authorization compares the
        // sender's ID, so there is no private/channel distinction to draw.
        direct: true,
        text: text.to_string(),
    };

    let reply = match cp.process_command(&request).await {
        Ok(lines) => lines,
        Err(e) => vec![e.to_string()],
    };

    if let Err(e) = slack.send_lines(&reply).await {
        error!("Could not reply over Slack: {e}");
    }
}

#[cfg(test)]
mod test {
    use super::*;

    const CHANNEL: &str = "C0CHANNEL";

    fn events_api(event: Value) -> String {
        json!({
            "type": "events_api",
            "envelope_id": "env-1",
            "payload": { "event": event },
        })
        .to_string()
    }

    #[test]
    fn a_plain_message_in_our_channel_is_a_command_candidate() {
        let raw = events_api(json!({
            "type": "message", "channel": CHANNEL, "user": "U0OWNER", "text": "lt!"
        }));

        match classify(&raw, CHANNEL) {
            Frame::Message { envelope_id, user, text } => {
                assert_eq!(envelope_id, "env-1");
                assert_eq!(user, "U0OWNER");
                assert_eq!(text, "lt!");
            }
            _ => panic!("should have been a message"),
        }
    }

    /// The bot must never read its own output. A reply can begin with something
    /// command-shaped, and answering it would loop until Slack rate-limited us.
    #[test]
    fn the_bots_own_messages_are_not_commands() {
        for event in [
            json!({"type":"message","channel":CHANNEL,"user":"U0BOT","text":"lt!","bot_id":"B01"}),
            json!({"type":"message","channel":CHANNEL,"user":"U0BOT","text":"lt!","subtype":"bot_message"}),
        ] {
            assert!(
                matches!(classify(&events_api(event), CHANNEL), Frame::AckOnly(_)),
                "a bot message must be acked but not run"
            );
        }
    }

    /// Edits and deletions arrive as messages with a subtype; re-running a
    /// command because someone fixed a typo would be surprising.
    #[test]
    fn edited_and_deleted_messages_are_ignored() {
        for subtype in ["message_changed", "message_deleted", "channel_join"] {
            let raw = events_api(json!({
                "type": "message", "channel": CHANNEL, "user": "U0OWNER",
                "text": "lt!", "subtype": subtype
            }));
            assert!(matches!(classify(&raw, CHANNEL), Frame::AckOnly(_)), "{subtype}");
        }
    }

    #[test]
    fn another_channel_is_ignored_but_still_acked() {
        let raw = events_api(json!({
            "type": "message", "channel": "C0ELSEWHERE", "user": "U0OWNER", "text": "lt!"
        }));
        assert!(matches!(classify(&raw, CHANNEL), Frame::AckOnly(_)));
    }

    /// Every envelope must be acked, including ones we do nothing with --
    /// unacknowledged envelopes are redelivered indefinitely.
    #[test]
    fn unhandled_envelope_types_are_still_acked() {
        let raw = json!({"type": "slash_commands", "envelope_id": "env-9"}).to_string();
        match classify(&raw, CHANNEL) {
            Frame::AckOnly(id) => assert_eq!(id, "env-9"),
            _ => panic!("should be acked"),
        }
    }

    #[test]
    fn hello_and_disconnect_are_recognised() {
        assert!(matches!(classify(r#"{"type":"hello"}"#, CHANNEL), Frame::Ignore));
        assert!(matches!(
            classify(r#"{"type":"disconnect","reason":"refresh_requested"}"#, CHANNEL),
            Frame::Disconnect
        ));
    }

    /// Garbage must not take the socket down.
    #[test]
    fn unparseable_frames_are_ignored() {
        assert!(matches!(classify("not json at all", CHANNEL), Frame::Ignore));
        assert!(matches!(classify("{}", CHANNEL), Frame::Ignore));
    }

    /// Slack's codes are terse; the hint is what turns them into a fix.
    #[test]
    fn common_errors_are_explained() {
        assert!(explain("not_in_channel").contains("/invite"));
        assert!(explain("not_allowed_token_type").contains("xapp-"));
        assert!(explain("channel_not_found").contains("channel_id"));
        // Anything unrecognised passes through rather than being swallowed.
        assert_eq!(explain("some_new_code"), "some_new_code");
    }

    fn client_with(base: &str, target: Target) -> Slack {
        Slack {
            http: reqwest::Client::builder().timeout(Duration::from_secs(5)).build().unwrap(),
            app_token: "xapp-test".into(),
            bot_token: "xoxb-test".into(),
            target,
            base: base.to_string(),
        }
    }

    fn client_against(base: &str) -> Slack {
        client_with(base, Target::Channel(CHANNEL.into()))
    }

    fn dm_client_against(base: &str) -> Slack {
        client_with(
            base,
            Target::Direct { owner_id: "U0OWNER".into(), resolved: Arc::new(OnceCell::new()) },
        )
    }

    /// One canned JSON reply per connection, returning what each request looked
    /// like. Same technique as the Telegram and ntfy tests: a real socket, so the
    /// request shape is checked against something rather than assumed.
    async fn fake_api(replies: Vec<&'static str>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());

        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for reply in replies {
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

        (base, server)
    }

    /// The two tokens must not be interchanged: Slack answers
    /// `not_allowed_token_type` if they are, and that error names neither of
    /// them. Assert each call carries the right one.
    #[tokio::test]
    async fn each_call_carries_the_token_it_is_meant_to() {
        let (base, server) = fake_api(vec![
            r#"{"ok":true,"url":"wss://wss-primary.slack.com/link/?ticket=abc"}"#,
            r#"{"ok":true,"ts":"1.2"}"#,
        ])
        .await;
        let slack = client_against(&base);

        let url = slack.open_socket().await.unwrap();
        assert_eq!(url, "wss://wss-primary.slack.com/link/?ticket=abc");
        slack.send("2 torrents (1 complete):").await.unwrap();

        let seen = server.await.unwrap();
        assert!(seen[0].starts_with("POST /apps.connections.open"), "{}", &seen[0][..40]);
        assert!(seen[0].contains("authorization: Bearer xapp-test"), "{}", seen[0]);

        assert!(seen[1].starts_with("POST /chat.postMessage"), "{}", &seen[1][..40]);
        assert!(seen[1].contains("authorization: Bearer xoxb-test"), "{}", seen[1]);
        assert!(seen[1].contains(CHANNEL), "the message must name the channel: {}", seen[1]);
        assert!(seen[1].contains("2 torrents"), "{}", seen[1]);
    }

    /// Slack signals almost every failure as HTTP 200 with `ok: false`, so the
    /// status code alone would report success for a revoked token.
    #[tokio::test]
    async fn an_ok_false_body_is_a_failure_despite_http_200() {
        let (base, _server) = fake_api(vec![r#"{"ok":false,"error":"not_in_channel"}"#]).await;

        let err = client_against(&base)
            .send("anything")
            .await
            .expect_err("ok:false must not be treated as success");

        // And the hint, not just the code, is what the operator needs.
        assert!(err.to_string().contains("/invite"), "{err}");
    }

    /// The socket half, over a real WebSocket: an envelope arrives, is
    /// classified, and the acknowledgement Slack requires goes back down the same
    /// connection. Without the ack Slack redelivers the event every few seconds.
    #[tokio::test]
    async fn an_envelope_is_acknowledged_over_the_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://127.0.0.1:{}/link", listener.local_addr().unwrap().port());

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            ws.send(WsMessage::Text(r#"{"type":"hello"}"#.into())).await.unwrap();
            ws.send(WsMessage::Text(
                events_api(json!({
                    "type": "message", "channel": CHANNEL, "user": "U0OWNER", "text": "lt!"
                }))
                .into(),
            ))
            .await
            .unwrap();

            // Whatever comes back is the ack.
            match ws.next().await.unwrap().unwrap() {
                WsMessage::Text(t) => t.to_string(),
                other => panic!("expected an ack, got {other:?}"),
            }
        });

        let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        let mut acked = false;
        while let Some(frame) = socket.next().await {
            let WsMessage::Text(raw) = frame.unwrap() else { continue };
            match classify(&raw, CHANNEL) {
                Frame::Message { envelope_id, user, text } => {
                    assert_eq!((user.as_str(), text.as_str()), ("U0OWNER", "lt!"));
                    ack(&mut socket, &envelope_id).await.unwrap();
                    acked = true;
                    break;
                }
                Frame::Ignore => {}
                other => panic!("unexpected frame: {}", matches!(other, Frame::Disconnect)),
            }
        }
        assert!(acked, "the message frame never arrived");

        let received = server.await.unwrap();
        assert_eq!(received, r#"{"envelope_id":"env-1"}"#);
    }

    /// The DM path, end to end: the conversation is opened by member ID, the
    /// `D…` id that comes back is what the message is posted to, and the second
    /// message does not open it again.
    #[tokio::test]
    async fn a_direct_message_resolves_the_conversation_once() {
        let (base, server) = fake_api(vec![
            r#"{"ok":true,"channel":{"id":"D0PRIVATE"}}"#,
            r#"{"ok":true,"ts":"1.2"}"#,
            r#"{"ok":true,"ts":"1.3"}"#,
        ])
        .await;
        let slack = dm_client_against(&base);

        slack.send("first").await.unwrap();
        slack.send("second").await.unwrap();

        let seen = server.await.unwrap();
        assert!(seen[0].starts_with("POST /conversations.open"), "{}", &seen[0][..40]);
        assert!(seen[0].contains("U0OWNER"), "opened by member id: {}", seen[0]);

        assert!(seen[1].starts_with("POST /chat.postMessage"), "{}", &seen[1][..40]);
        assert!(seen[1].contains("D0PRIVATE"), "posted to the DM: {}", seen[1]);
        // Only three requests were served; a second conversations.open would
        // have taken the slot the second message needed.
        assert!(seen[2].contains("D0PRIVATE") && seen[2].contains("second"), "{}", seen[2]);
    }

    /// A failed open must not be cached as success or as failure: the network
    /// being down for a moment should not leave Slack permanently mute.
    #[tokio::test]
    async fn a_failed_conversation_open_is_retried() {
        let (base, server) = fake_api(vec![
            r#"{"ok":false,"error":"ratelimited"}"#,
            r#"{"ok":true,"channel":{"id":"D0PRIVATE"}}"#,
            r#"{"ok":true,"ts":"1.2"}"#,
        ])
        .await;
        let slack = dm_client_against(&base);

        assert!(slack.send("first").await.is_err());
        slack.send("second").await.expect("the retry should succeed");

        let seen = server.await.unwrap();
        assert!(seen[1].starts_with("POST /conversations.open"), "{}", &seen[1][..40]);
        assert!(seen[2].contains("D0PRIVATE"), "{}", seen[2]);
    }

    #[test]
    fn an_incomplete_section_disables_the_transport() {
        use crate::config::config::{EventFilter, SlackOptions};

        let base = SlackOptions {
            app_token: "xapp-1".into(),
            bot_token: "xoxb-1".into(),
            channel_id: Some("C1".into()),
            owner_id: "U1".into(),
            commands: true,
            notifications: true,
            events: EventFilter::default(),
        };

        assert!(Slack::new(&base).is_some());
        for broken in [
            SlackOptions { app_token: " ".into(), ..base.clone() },
            SlackOptions { bot_token: "".into(), ..base.clone() },
            SlackOptions { owner_id: "".into(), ..base.clone() },
        ] {
            assert!(Slack::new(&broken).is_none());
        }
    }

    /// No channel_id is the private setup, not a broken one. An empty string is
    /// a half-finished edit and must mean the same thing rather than posting to
    /// a channel named "".
    #[test]
    fn an_absent_channel_means_a_direct_message() {
        use crate::config::config::{EventFilter, SlackOptions};

        for channel_id in [None, Some(String::new()), Some("  ".to_string())] {
            let slack = Slack::new(&SlackOptions {
                app_token: "xapp-1".into(),
                bot_token: "xoxb-1".into(),
                channel_id,
                owner_id: "U0OWNER".into(),
                commands: true,
                notifications: true,
                events: EventFilter::default(),
            })
            .expect("a DM setup is complete without a channel");

            match slack.target {
                Target::Direct { owner_id, .. } => assert_eq!(owner_id, "U0OWNER"),
                Target::Channel(c) => panic!("should be a DM, got channel {c}"),
            }
        }
    }
}
