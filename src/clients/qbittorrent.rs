//! qBittorrent, over its WebUI API.
//!
//! The first client besides rTorrent to implement all three things the bot
//! needs -- adding, listing and reporting completion. Flood manages only the
//! first, which is why `torrentlist` and download-finished notifications have
//! been rTorrent-only until now.
//!
//! Plain HTTPS with a session cookie, so `reqwest` covers it with no new
//! dependency. Two things about the API shape are worth knowing before reading
//! on:
//!
//!   * **403 means "log in again", not "forbidden".** It is how qBittorrent
//!     reports an absent or expired `SID`, on every endpoint. One helper
//!     (`send_authed`) owns that retry so it cannot drift between call sites the
//!     way Flood's two hand-copied versions already have.
//!   * **WebAPI 2.11.0 (qBittorrent 5.0) renamed `paused` to `stopped`.** Alpine
//!     ships 5.2.1, so our own image is on the new spelling, but a NAS package
//!     may well be 4.x. `api_at_least` decides, numerically.

use std::time::Duration;

use anyhow::Error;
use base64::{engine::general_purpose, Engine};
use log::{error, info};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::Value;

use crate::announce::Announce;
use crate::clients::{CompletionRow, TorrentInfo, Unrecoverable};
use crate::config::config::QBittorrentOptions;
use crate::template::TextTemplate;

/// Long enough for a slow client on a busy NAS, short enough that a half-open
/// connection does not take the bot down with it.
///
/// Not optional. `notify::poll` calls `get_completed` on the *same task* as the
/// IRC listener, so a stalled request here stops the client answering server
/// PINGs and gets it disconnected. Flood's client sets no timeout at all.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Talks to one qBittorrent WebUI.
pub struct QBittorrent {
    /// Holds the `SID` cookie itself, behind its own lock -- which is why
    /// nothing here needs a `RefCell` despite every method taking `&self`.
    client: reqwest::Client,
    /// Base URL with any trailing slash removed, so every call site can format
    /// `{url}/api/v2/...` without producing a double slash.
    url: String,
    username: String,
    password: String,
    /// Empty means "wherever qBittorrent is configured to put things".
    save_path: String,
    /// Empty means no category.
    category: String,
    /// Compiled once here rather than per add, and `clients` already needs a
    /// restart to change, so they inherit that documented behaviour.
    tags_template: Option<TextTemplate>,
    category_template: Option<TextTemplate>,
    /// The WebAPI version as reported once at construction, e.g. `2.11.0`.
    ///
    /// Cached rather than asked per call: it decides the name of one form field
    /// on every add, and re-asking would double the round trips for a value
    /// that cannot change without the server restarting.
    api_version: String,
}

// Hand-written rather than derived, for the same reason `QBittorrentOptions`
// has one: a derived Debug would put the WebUI password in any log line or
// panic message that happens to format this.
impl std::fmt::Debug for QBittorrent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QBittorrent")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("api_version", &self.api_version)
            .finish()
    }
}

impl QBittorrent {
    pub async fn new(options: &QBittorrentOptions) -> Result<QBittorrent, Error> {
        let url = options.url.trim().trim_end_matches('/').to_string();
        if url.is_empty() {
            return Err(Unrecoverable("[clients.qBittorrent] needs a `url`".into()).into());
        }

        let client = reqwest::Client::builder()
            .cookie_store(true)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("irc2torrent/", env!("CARGO_PKG_VERSION")))
            // Deliberately no Origin or Referer: qBittorrent's CSRF check only
            // rejects a request that carries one and disagrees with Host, and
            // reqwest sends neither.
            .build()
            .map_err(|e| Error::msg(format!("Could not build the qBittorrent client: {e}")))?;

        // Parsed here so a malformed template is a startup error rather than a
        // surprise on the first add. Empty means "not configured", which is
        // distinct from a template that renders empty.
        let compile = |t: &str, what: &str| -> Result<Option<TextTemplate>, Error> {
            if t.trim().is_empty() {
                return Ok(None);
            }
            TextTemplate::parse(t, what).map(Some)
        };

        let mut qbt = Self {
            client,
            url,
            username: options.username.clone(),
            password: options.password.clone(),
            save_path: options.save_path.clone(),
            category: options.category.clone(),
            tags_template: compile(&options.tags_template, "[clients.qBittorrent] tags_template")?,
            category_template: compile(
                &options.category_template,
                "[clients.qBittorrent] category_template",
            )?,
            api_version: String::new(),
        };

        // Note what this does *not* do: log in. The version probe goes through
        // `send_authed`, which logs in only if the server answers 403. So there
        // is one code path that establishes a session, the connectivity check
        // and the version probe are the same request, a wrong password costs
        // exactly one failed login rather than five, and an install with
        // authentication bypassed for localhost -- which is what our own image
        // ships -- works with empty credentials and never logs in at all.
        qbt.api_version = qbt.probe_version().await?;
        info!(
            "Connected to qBittorrent, WebAPI {} at {}",
            qbt.api_version, qbt.url
        );

        Ok(qbt)
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/api/v2/{path}", self.url)
    }

    async fn probe_version(&self) -> Result<String, Error> {
        let response = self
            .send_authed("webapiVersion", || {
                self.client.get(self.endpoint("app/webapiVersion"))
            })
            .await?;

        if !response.status().is_success() {
            return Err(Error::msg(format!(
                "qBittorrent returned HTTP {} for its API version",
                response.status().as_u16()
            )));
        }

        let version = response
            .text()
            .await
            .map_err(|e| transport_error("webapiVersion", e))?;
        Ok(version.trim().to_string())
    }

    /// Send a request, and if qBittorrent answers 403 -- its way of saying the
    /// session cookie is missing or expired -- log in once and send it again.
    ///
    /// The retry lives here and nowhere else. `flood.rs` carries two hand-copied
    /// versions of the same idea and they have already diverged: its add path
    /// returns on any non-2xx status *before* reaching the re-login check, so an
    /// expired Flood session is reported to the user as a failure and never
    /// actually retried. Keying off the status code inside one helper makes that
    /// shape unrepresentable.
    ///
    /// It also never reads the body, so unlike Flood's
    /// `response_text.contains("Unauthorized")` it cannot be fooled by a torrent
    /// that happens to be named "Unauthorized".
    ///
    /// `build` is a closure rather than a cloned `RequestBuilder` because that
    /// type is not `Clone` for every body shape, and rebuilding costs nothing.
    ///
    /// Two concurrent 403s produce two logins. Harmless -- the last `SID` wins
    /// and both retries carry a valid cookie -- and not worth a lock to prevent.
    async fn send_authed<F>(&self, what: &str, build: F) -> Result<reqwest::Response, Error>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let response = build()
            .send()
            .await
            .map_err(|e| transport_error(what, e))?;

        if response.status() != StatusCode::FORBIDDEN {
            return Ok(response);
        }

        self.login().await?;

        let retried = build()
            .send()
            .await
            .map_err(|e| transport_error(what, e))?;

        if retried.status() == StatusCode::FORBIDDEN {
            return Err(Error::msg("qBittorrent refused the session"));
        }
        Ok(retried)
    }

    /// The only place a session is established.
    async fn login(&self) -> Result<(), Error> {
        let response = self
            .client
            .post(self.endpoint("auth/login"))
            .form(&[
                ("username", self.username.as_str()),
                ("password", self.password.as_str()),
            ])
            .send()
            .await
            .map_err(|e| transport_error("login", e))?;

        let status = response.status();
        // Never `{:?}` the response: it carries the freshly issued session
        // cookie, and this line goes to syslog.
        if status == StatusCode::FORBIDDEN {
            return Err(Unrecoverable(
                "qBittorrent has banned this IP after repeated failed logins".into(),
            )
            .into());
        }

        let body = response
            .text()
            .await
            .map_err(|e| transport_error("login", e))?;

        // Accept only the documented success sentinel. qBittorrent answers
        // `Fails.` for bad credentials today, but matching on the failure string
        // would break the day that wording changes; matching on success will not.
        if status.is_success() && body.trim() == "Ok." {
            return Ok(());
        }

        Err(Unrecoverable("qBittorrent rejected the WebUI username or password".into()).into())
    }

    /// Every torrent, with enough detail for a line each.
    pub(crate) async fn get_torrent_info(&self) -> Result<Vec<TorrentInfo>, Error> {
        Ok(self
            .torrents()
            .await?
            .iter()
            .map(QbtTorrent::to_info)
            .collect())
    }

    /// Hash, name and whether it has finished, for the completion poller.
    pub(crate) async fn get_completion(&self) -> Result<Vec<CompletionRow>, Error> {
        Ok(self
            .torrents()
            .await?
            .into_iter()
            .map(|t| CompletionRow {
                complete: t.is_complete(),
                hash: t.hash,
                name: t.name,
            })
            .collect())
    }

    async fn torrents(&self) -> Result<Vec<QbtTorrent>, Error> {
        let response = self
            .send_authed("torrents/info", || {
                self.client.get(self.endpoint("torrents/info"))
            })
            .await?;

        if !response.status().is_success() {
            return Err(Error::msg(format!(
                "qBittorrent returned HTTP {} for the torrent list",
                response.status().as_u16()
            )));
        }

        response
            .json::<Vec<QbtTorrent>>()
            .await
            .map_err(|e| transport_error("torrents/info", e))
    }

    /// Upload a `.torrent` and start it.
    ///
    /// `file` is base64, which is the contract `TorrentProcessor` hands every
    /// client -- the bot fetches the bytes itself and passes them along.
    pub(crate) async fn add_torrent_and_start(
        &self,
        file: &str,
        announce: &Announce,
    ) -> Result<(), Error> {
        let name = announce.name.clone();
        let bytes = general_purpose::STANDARD
            .decode(file.as_bytes())
            .map_err(|_| Error::msg("The torrent file was not valid base64"))?;

        // Parse before uploading, discarding the result. It turns "the tracker
        // served an HTML login page instead of a torrent" into a local message
        // naming the release, rather than a bare HTTP 415 from qBittorrent.
        // Same wording as the rTorrent client, so the two report it identically.
        if let Err(e) = lava_torrent::torrent::v1::Torrent::read_from_bytes(&bytes) {
            return Err(Error::msg(format!(
                "Torrent ({name}) could not be parsed: {e}"
            )));
        }

        // `/torrents/add` with neither field honours the user's global "add
        // torrents in a stopped state" preference, so a bare add can sit paused
        // forever. This method is called add_torrent_and_*start*, so say so.
        let start_field = if api_at_least(&self.api_version, 2, 11) {
            "stopped"
        } else {
            "paused"
        };

        // Rendered before the field list is built so the strings outlive it.
        // An empty render means "nothing to say", not "set it to empty": a
        // release with no `uploader` capture gets one fewer tag rather than a
        // blank one, matching how the fixed `category` behaves.
        let tags = self
            .tags_template
            .as_ref()
            .map(|t| sanitize_field(&t.render(announce)))
            .unwrap_or_default();
        let rendered_category = self
            .category_template
            .as_ref()
            .map(|t| sanitize_field(&t.render(announce)))
            .unwrap_or_default();

        let mut fields: Vec<(&str, &str)> = vec![(start_field, "false")];
        if !self.save_path.is_empty() {
            fields.push(("savepath", &self.save_path));
        }
        // The template wins when it produced something, so the fixed value is
        // the fallback for releases whose captures did not fire.
        let category = if rendered_category.is_empty() {
            self.category.as_str()
        } else {
            rendered_category.as_str()
        };
        if !category.is_empty() {
            fields.push(("category", category));
        }
        if !tags.is_empty() {
            fields.push(("tags", &tags));
        }

        let boundary = format!(
            "----irc2torrent{:016x}{:016x}",
            fastrand::u64(..),
            fastrand::u64(..)
        );
        let body = multipart_body(&boundary, &bytes, &fields);
        let content_type = format!("multipart/form-data; boundary={boundary}");

        let response = self
            .send_authed("torrents/add", || {
                self.client
                    .post(self.endpoint("torrents/add"))
                    .header(reqwest::header::CONTENT_TYPE, &content_type)
                    .body(body.clone())
            })
            .await?;

        let status = response.status();
        if status == StatusCode::UNSUPPORTED_MEDIA_TYPE {
            return Err(Error::msg(
                "qBittorrent rejected the file as not a valid torrent",
            ));
        }

        // Read the body even on failure. qBittorrent 5.x answers 409 with the
        // actual reason in plain text -- "Save path is not writable", and so on
        // -- and reporting the bare status code sends the user hunting for a
        // meaning the response already carried. It is the server's own message,
        // so it holds nothing of ours; it is trimmed only to fit one IRC line.
        let body = response
            .text()
            .await
            .map_err(|e| transport_error("torrents/add", e))?;

        // 409 is overwhelmingly "you already have this one" -- qBittorrent logs
        // `Detected an attempt to add a duplicate torrent` and answers with a
        // body of literally "Conflict", which tells the user nothing. It uses
        // the same status for other add failures, so this cannot be reported as
        // success, but it can at least name the likely cause instead of leaving
        // a bare status code in an IRC channel.
        if status == StatusCode::CONFLICT {
            return Err(Error::msg("qBittorrent refused it; most likely already added"));
        }
        if !status.is_success() {
            return Err(Error::msg(format!(
                "qBittorrent returned HTTP {}{}",
                status.as_u16(),
                reason(&body)
            )));
        }
        // Success is not one fixed string. qBittorrent 4.x answers `Ok.`; 5.x
        // answers JSON -- `{"added_torrent_ids":["<hash>"], ...}` -- and the
        // published API docs still describe only the former. Requiring `Ok.`
        // meant every add against a 5.x server was reported as a failure
        // *after* succeeding, which is worse than either outcome alone.
        //
        // So: a 2xx is success unless the body actively says otherwise. That
        // survives the next change of wording, which is evidently a thing that
        // happens here.
        if body.trim().eq_ignore_ascii_case("fails.") {
            return Err(Error::msg("qBittorrent did not accept the torrent"));
        }
        if let Ok(json) = serde_json::from_str::<Value>(&body) {
            if json.get("added_torrent_ids").and_then(Value::as_array).is_some_and(Vec::is_empty) {
                return Err(Error::msg("qBittorrent accepted the request but added nothing"));
            }
        }

        info!("Added {name} to qBittorrent.");
        Ok(())
    }
}

/// One entry of `GET /api/v2/torrents/info`.
///
/// `#[serde(default)]` at the container level because qBittorrent adds fields
/// between versions: a 4.3 server predates several of these, and one missing key
/// must not fail the whole list. Unknown keys are ignored, which is what makes
/// this forward-compatible too.
///
/// `hash` rather than `infohash_v1`, which only exists from 4.4.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct QbtTorrent {
    hash: String,
    name: String,
    /// Size of the *selected* files. 0 for a magnet whose metadata has not
    /// arrived yet.
    size: i64,
    /// Downloaded bytes of the selected files.
    completed: i64,
    ratio: f64,
    state: String,
}

impl QbtTorrent {
    fn to_info(&self) -> TorrentInfo {
        TorrentInfo {
            name: self.name.clone(),
            size_bytes: self.size.max(0),
            // Not clamped to `size`: `TorrentInfo::percent_done` already caps at
            // 100 and `is_complete` uses `>=`, so a `completed` that overshoots
            // -- qBittorrent counts a partially verified piece -- needs no help.
            completed_bytes: self.completed.max(0),
            ratio_permille: ratio_permille(self.ratio),
        }
    }

    /// Whether qBittorrent considers this torrent finished downloading.
    ///
    /// Two signals, OR'd, because each is stable exactly where the other flaps
    /// -- and flapping is not cosmetic here. `notify::poll` does
    /// `finished.retain(|h| current.contains(h))`, so a hash that drops out of
    /// the complete set for a single tick is forgotten and then re-announced as
    /// newly finished when it comes back.
    ///
    ///   * The **bytes** hold through `moving` (post-completion relocation),
    ///     through a paused seed, and through an error raised after completion.
    ///   * The **state** holds through a recheck, where `completed` is walked
    ///     back to zero and counted up again while the state stays `checkingUP`.
    ///
    /// The state list names `pausedUP` *and* `stoppedUP`, so it spans both API
    /// generations with no version gate. `uploading` is listed explicitly
    /// because, unlike its siblings, it does not end in "UP".
    ///
    /// The byte half is deliberately the same test as `TorrentInfo::is_complete`
    /// and as rTorrent's `d.complete`, so both clients feed the poller the same
    /// meaning.
    fn is_complete(&self) -> bool {
        if self.size > 0 && self.completed >= self.size {
            return true;
        }
        matches!(
            self.state.as_str(),
            "uploading"
                | "stalledUP"
                | "queuedUP"
                | "checkingUP"
                | "forcedUP"
                | "pausedUP"
                | "stoppedUP"
        )
    }
}

/// qBittorrent reports a float ratio; `TorrentInfo` wants per mille, the way
/// rTorrent's `d.ratio` already gives it.
///
/// `-1` is qBittorrent's sentinel for an infinite ratio (uploaded something,
/// downloaded nothing). `TorrentInfo` has no way to say that, and 0 is the safer
/// lie: the value only ever reaches a cosmetic `summary()` line, whereas a huge
/// number would read as a real measurement.
fn ratio_permille(ratio: f64) -> i64 {
    if !ratio.is_finite() || ratio <= 0.0 {
        return 0;
    }
    (ratio * 1000.0).round().min(i64::MAX as f64) as i64
}

/// Whether a dotted WebAPI version is at least `major.minor`.
///
/// Numeric, not lexical: `"2.9.3" < "2.11.0"` is *false* as a string comparison,
/// which is exactly how a naive gate ends up sending the 5.x field name to every
/// 4.x server. An unparseable version answers false, so the older spelling wins
/// by default.
fn api_at_least(version: &str, major: u32, minor: u32) -> bool {
    let mut parts = version.trim().split('.').map(str::parse::<u32>);
    let (Some(Ok(found_major)), Some(Ok(found_minor))) = (parts.next(), parts.next()) else {
        return false;
    };
    (found_major, found_minor) >= (major, minor)
}

/// A `multipart/form-data` body with one file part named `torrents`.
///
/// Hand-built rather than using reqwest's `multipart`, which is behind a feature
/// this crate does not enable and which would pull `mime_guess` and `unicase`
/// into a dependency tree the manifest is deliberately strict about. Being a
/// plain `Vec<u8>` also makes it cheap to rebuild for the re-auth retry.
/// Longest field value sent. Generous for a tag list, short enough that a
/// pathological release name cannot bloat the request.
const MAX_FIELD_LEN: usize = 128;

/// Make a rendered value safe to put in a multipart field.
///
/// `multipart_body` writes values verbatim, and these are built from an IRC
/// announce line -- the same reason the torrent filename below is a constant.
/// The boundary is 128 bits of `fastrand`, so part injection is not a practical
/// worry, but "cannot inject" should be structural rather than probabilistic.
fn sanitize_field(value: &str) -> String {
    let cleaned: String = value.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    match cleaned.char_indices().nth(MAX_FIELD_LEN) {
        // Truncate on a char boundary, not a byte one.
        Some((byte, _)) => cleaned[..byte].to_string(),
        None => cleaned,
    }
}

fn multipart_body(boundary: &str, torrent: &[u8], fields: &[(&str, &str)]) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::with_capacity(torrent.len() + 512);

    for (key, value) in fields {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{key}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }

    // The filename is a constant, deliberately. The release name comes off an
    // IRC announce line and is attacker-influenced -- a name carrying CRLF would
    // be header injection into our own request. qBittorrent only uses the
    // filename for its own logging, so there is nothing to lose by fixing it.
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"torrents\"; \
             filename=\"torrent\"\r\nContent-Type: application/x-bittorrent\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(torrent);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    body
}

/// The server's own explanation, if it gave one, as a suffix.
///
/// Trimmed to one line and 60 characters: these strings are appended to a
/// message that goes verbatim to an IRC channel, where the whole line has 380
/// bytes to live in.
fn reason(body: &str) -> String {
    let text = body.trim().lines().next().unwrap_or_default().trim();
    if text.is_empty() {
        return String::new();
    }
    let short: String = text.chars().take(60).collect();
    format!(": {short}")
}

/// Summarise a transport failure without leaking where it was going.
///
/// `reqwest::Error`'s `Display` embeds the request URL, and this string is
/// carried by `TorrentOutcome::AddFailed` straight to an IRC channel, a Telegram
/// chat or a Slack channel. Log the detail, return the summary.
fn transport_error(what: &str, e: reqwest::Error) -> Error {
    error!("qBittorrent {what} failed: {e}");
    Error::msg("qBittorrent is unreachable")
}

#[cfg(test)]
mod test {
    use super::*;

    fn torrent(state: &str, size: i64, completed: i64) -> QbtTorrent {
        QbtTorrent {
            hash: "abc123".into(),
            name: "Some Release".into(),
            size,
            completed,
            ratio: 0.0,
            state: state.into(),
        }
    }

    /// A magnet whose metadata has not arrived has no size, and dividing by it
    /// would panic.
    #[test]
    fn a_metadataless_magnet_reads_as_zero_percent() {
        let info = torrent("metaDL", 0, 0).to_info();
        assert_eq!(info.percent_done(), 0);
        assert!(!info.is_complete());
    }

    /// `-1` is qBittorrent's "infinite ratio". Reporting it as -0.00, or as a
    /// huge number, would both read as a real measurement.
    #[test]
    fn the_infinite_ratio_sentinel_becomes_zero() {
        assert_eq!(ratio_permille(-1.0), 0);
        assert_eq!(ratio_permille(f64::NAN), 0);
        assert_eq!(ratio_permille(f64::INFINITY), 0);
        assert_eq!(ratio_permille(0.0), 0);
    }

    #[test]
    fn a_ratio_is_carried_across_as_per_mille() {
        assert_eq!(ratio_permille(1.2345), 1235);
        assert_eq!(ratio_permille(0.5), 500);
    }

    /// qBittorrent counts a partially verified piece, so `completed` can
    /// overshoot. The existing helpers already cope; this pins that they do.
    #[test]
    fn completed_beyond_size_still_reads_as_done() {
        let info = torrent("uploading", 100, 120).to_info();
        assert_eq!(info.percent_done(), 100);
        assert!(info.is_complete());
    }

    /// The duplicate-notification guard, and the reason completion is not a
    /// pure byte test: a recheck walks `completed` back to zero, and a torrent
    /// that leaves the finished set for one tick is re-announced when it
    /// returns.
    #[test]
    fn a_recheck_does_not_unfinish_a_seeding_torrent() {
        assert!(torrent("checkingUP", 100, 0).is_complete());
    }

    /// And the reason it is not a pure state test: `moving` and `error` are
    /// neither "UP" nor "DL", so a finished torrent being relocated would
    /// silently drop out.
    #[test]
    fn a_relocating_or_errored_finished_torrent_is_still_finished() {
        assert!(torrent("moving", 100, 100).is_complete());
        assert!(torrent("error", 100, 100).is_complete());
    }

    /// qBittorrent 5.0 renamed the paused states. Both spellings must count, or
    /// the answer depends on which version the user happens to run.
    #[test]
    fn both_api_generations_agree_a_paused_seed_is_done() {
        assert!(torrent("pausedUP", 0, 0).is_complete());
        assert!(torrent("stoppedUP", 0, 0).is_complete());
    }

    #[test]
    fn a_download_in_progress_is_not_finished() {
        for state in ["downloading", "stalledDL", "queuedDL", "metaDL", "pausedDL"] {
            assert!(!torrent(state, 100, 5).is_complete(), "{state}");
        }
    }

    /// `"2.9.3" < "2.11.0"` is false as a string comparison, which is how a
    /// naive gate sends the wrong field name to every 4.x server.
    #[test]
    fn api_at_least_orders_numerically_not_lexically() {
        assert!(!api_at_least("2.9.3", 2, 11));
        assert!(api_at_least("2.11.0", 2, 11));
        assert!(api_at_least("2.11", 2, 11));
        assert!(api_at_least("3.0.1", 2, 11));
        assert!(api_at_least(" 2.11.0\n", 2, 11));
        // Unparseable falls back to the older spelling rather than guessing.
        assert!(!api_at_least("", 2, 11));
        assert!(!api_at_least("banana", 2, 11));
        assert!(!api_at_least("2", 2, 11));
    }

    /// A server older or newer than we know about must not fail the whole list
    /// over one field.
    #[test]
    fn unknown_and_missing_fields_do_not_break_the_list() {
        let json = r#"[
            {"hash":"a","name":"One","size":10,"completed":10,"ratio":1.0,
             "state":"uploading","some_future_field":42},
            {"hash":"b","name":"Two"}
        ]"#;
        let parsed: Vec<QbtTorrent> = serde_json::from_str(json).expect("must parse");

        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].is_complete());
        // The sparse one degrades to zeroes rather than failing.
        assert_eq!(parsed[1].size, 0);
        assert!(!parsed[1].is_complete());
    }

    /// The release name comes off an IRC announce line. Interpolating it into a
    /// MIME header would be header injection into our own request.
    #[test]
    fn a_hostile_torrent_name_cannot_reach_a_mime_header() {
        let body = multipart_body(
            "BOUND",
            b"d8:announce4:teste",
            &[("savepath", "/downloads")],
        );
        let text = String::from_utf8_lossy(&body);

        // Exactly one part header per field, and the file part's filename is the
        // fixed one rather than anything derived from the release.
        assert_eq!(text.matches("name=\"savepath\"").count(), 1);
        assert_eq!(text.matches("name=\"torrents\"").count(), 1);
        assert!(text.contains("filename=\"torrent\""), "{text}");
    }

    /// Tag values are built from an IRC announce line, so they reach
    /// `multipart_body` from the same untrusted place the release name does.
    #[test]
    fn a_field_value_cannot_carry_crlf_into_a_mime_header() {
        let hostile = "Movies\r\nContent-Disposition: form-data; name=\"savepath\"\r\n\r\n/etc";
        let cleaned = sanitize_field(hostile);
        assert!(!cleaned.contains('\r') && !cleaned.contains('\n'), "{cleaned:?}");

        let body = multipart_body("BOUND", b"d8:announce4:teste", &[("tags", &cleaned)]);
        let text = String::from_utf8_lossy(&body);

        // The injected text survives as *text* -- that is fine and expected.
        // What matters is that it cannot become a header, which takes the CRLF
        // that is no longer there. Counting the delimiter, not the words.
        assert_eq!(
            text.matches("\r\nContent-Disposition").count(),
            2,
            "only the tags part and the file part are headers:\n{text}"
        );
        assert_eq!(
            text.matches("\r\nContent-Disposition: form-data; name=\"savepath\"").count(),
            0,
            "the injected header must not be a real one:\n{text}"
        );
    }

    #[test]
    fn a_field_value_is_capped_on_a_char_boundary() {
        // Multi-byte on purpose: truncating by bytes would split one.
        let long = "é".repeat(MAX_FIELD_LEN + 50);
        let cleaned = sanitize_field(&long);
        assert_eq!(cleaned.chars().count(), MAX_FIELD_LEN);
        assert!(cleaned.is_char_boundary(cleaned.len()));
    }

    #[test]
    fn tags_and_category_are_rendered_from_captured_fields() {
        let re = regex::Regex::new(
            r"(?P<name>[^|]+)\|(?P<category>[^|]+)\|(?P<uploader>[^|]+)\|(?P<id>\d+)",
        )
        .unwrap();
        let caps = re.captures("Some.Release|Movies|j3rico|7").unwrap();
        let announce =
            Announce::from_captures(&re, &caps, &Default::default()).unwrap();

        let tags = TextTemplate::parse("{category},{uploader}", "tags_template").unwrap();
        assert_eq!(sanitize_field(&tags.render(&announce)), "Movies,j3rico");

        let category = TextTemplate::parse("{category}", "category_template").unwrap();
        assert_eq!(sanitize_field(&category.render(&announce)), "Movies");
    }

    #[test]
    fn a_template_whose_fields_are_absent_renders_nothing_to_send() {
        // An empty render means the field is omitted entirely, so a release
        // without the capture gets one fewer tag rather than a blank one.
        let announce = named("Some.Release");
        let tags = TextTemplate::parse("{uploader}", "tags_template").unwrap();
        assert!(sanitize_field(&tags.render(&announce)).is_empty());
    }

    #[test]
    fn a_malformed_template_is_rejected_when_the_client_is_built() {
        let mut o = opts("http://127.0.0.1:1".into(), String::new(), String::new(), String::new());
        o.tags_template = "{a-b}".into();
        // Fails on the template before any network call is attempted.
        let err = futures::executor::block_on(QBittorrent::new(&o)).unwrap_err().to_string();
        assert!(err.contains("tags_template"), "{err}");
    }

    #[test]
    fn the_multipart_body_carries_the_torrent_bytes_verbatim() {
        let bytes = b"d8:announce4:test4:infod6:lengthi1ee e";
        let body = multipart_body("BOUND", bytes, &[]);

        assert!(
            body.windows(bytes.len()).any(|w| w == bytes),
            "the torrent must survive unmodified"
        );
        assert!(String::from_utf8_lossy(&body).ends_with("--BOUND--\r\n"));
    }

    // -----------------------------------------------------------------------
    // Against a real socket
    // -----------------------------------------------------------------------

    /// One canned reply per connection, returning what each request looked like.
    ///
    /// The same technique as the ntfy, Telegram and Slack tests, with two
    /// changes qBittorrent needs: replies carry a **status code**, because 403
    /// is how it asks for a login and half of what follows is about that; and
    /// every reply carries `Connection: close`, because otherwise reqwest may
    /// hand the next request to a pooled connection the server has already
    /// finished with and the accept loop hangs.
    async fn fake_api(
        replies: Vec<(u16, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());

        let server = tokio::spawn(async move {
            let mut seen: Vec<Vec<u8>> = Vec::new();
            for (status, reply) in replies {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 65536];
                let n = sock.read(&mut buf).await.unwrap();
                seen.push(buf[..n].to_vec());

                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{}",
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

    fn text(request: &[u8]) -> String {
        String::from_utf8_lossy(request).to_string()
    }

    /// Options for the constructor, which takes the whole struct rather than
    /// seven positional strings.
    fn opts(url: String, username: String, password: String, category: String) -> QBittorrentOptions {
        QBittorrentOptions {
            url,
            username,
            password,
            save_path: String::new(),
            category,
            tags_template: String::new(),
            category_template: String::new(),
        }
    }

    /// The announce an add is made from, when the test only cares about a name.
    fn named(name: &str) -> Announce {
        Announce::new(name.to_string(), "1".to_string())
    }

    /// A client that skips `new()`, so a test can pin the API version rather
    /// than having to serve a probe first.
    fn client_against(base: &str, api_version: &str) -> QBittorrent {
        QBittorrent {
            client: reqwest::Client::builder().cookie_store(true).build().unwrap(),
            url: base.to_string(),
            username: "user".into(),
            password: "hunter2".into(),
            save_path: String::new(),
            category: String::new(),
            tags_template: None,
            category_template: None,
            api_version: api_version.to_string(),
        }
    }

    /// A minimal but genuinely valid single-file torrent, since the add path
    /// parses before uploading.
    ///
    /// Built with `lava_torrent` rather than hand-written bencode: an earlier
    /// hand-rolled version was subtly malformed, and a test fixture that is
    /// wrong in the same way the code under test would be is no test at all.
    ///
    /// The tracker is a dead loopback UDP port, so nothing announces for real
    /// if this ever reaches a live client.
    fn a_torrent() -> Vec<u8> {
        lava_torrent::torrent::v1::Torrent {
            announce: Some("udp://127.0.0.1:1/a".to_string()),
            announce_list: None,
            length: 4,
            files: None,
            name: "test".to_string(),
            piece_length: 16384,
            // 0xff, not 0x00. lava_torrent's decoder decides between a bencode
            // byte string and a text string by whether the bytes are valid
            // UTF-8 -- and twenty NUL bytes are -- so a zeroed hash comes back
            // as a String and the parse fails with `"pieces" does not map to a
            // sequence of bytes`. It cannot read its own output. 0xff is not
            // valid UTF-8, which is also true of a real SHA-1 hash often enough
            // that this never shows up in the field.
            pieces: vec![vec![0xffu8; 20]],
            extra_fields: None,
            extra_info_fields: None,
        }
        .encode()
        .expect("the fixture must be a valid torrent")
    }

    #[test]
    /// The fixture must survive the same parse the add path performs.
    ///
    /// Worth its own test because it has already been wrong once, and a broken
    /// fixture fails four other tests with a message that points at the client
    /// rather than at itself.
    fn the_fixture_is_a_torrent_the_add_path_would_accept() {
        let bytes = a_torrent();
        lava_torrent::torrent::v1::Torrent::read_from_bytes(&bytes)
            .expect("the fixture must parse");
    }

    fn a_torrent_b64() -> String {
        general_purpose::STANDARD.encode(a_torrent())
    }

    /// The shipped image bypasses authentication for localhost, so the common
    /// case must involve no login at all -- and must not be reached by trying
    /// one and failing.
    #[tokio::test]
    async fn no_login_happens_when_the_api_answers_unauthenticated() {
        let (base, server) = fake_api(vec![(200, "2.11.0")]).await;

        let qbt = QBittorrent::new(&opts(base.clone(), String::new(), String::new(), String::new()))
            .await
            .expect("an unauthenticated server should just work");
        assert_eq!(qbt.api_version, "2.11.0");

        let seen = server.await.unwrap();
        assert_eq!(seen.len(), 1, "one request, no login");
        assert!(text(&seen[0]).starts_with("GET /api/v2/app/webapiVersion"), "{}", text(&seen[0]));
    }

    /// And when it does need one, exactly one -- qBittorrent bans an IP after
    /// five failures.
    #[tokio::test]
    async fn a_403_at_construction_triggers_exactly_one_login() {
        let (base, server) =
            fake_api(vec![(403, "Unauthorized"), (200, "Ok."), (200, "2.11.0")]).await;

        QBittorrent::new(&opts(base, "user".into(), "pw".into(), String::new()))
            .await
            .expect("login then retry should succeed");

        let seen = server.await.unwrap();
        assert_eq!(seen.len(), 3);
        assert!(text(&seen[1]).starts_with("POST /api/v2/auth/login"), "{}", text(&seen[1]));
        assert!(text(&seen[1]).contains("username=user&password=pw"), "{}", text(&seen[1]));
        assert!(text(&seen[2]).starts_with("GET /api/v2/app/webapiVersion"));
    }

    /// The bug this design exists to prevent: flood.rs returns on a non-2xx
    /// *before* its re-login check, so an expired session is reported to the
    /// user and never retried. Here the call must be replayed intact.
    #[tokio::test]
    async fn an_expired_session_is_renewed_and_the_call_replayed() {
        let (base, server) = fake_api(vec![
            (403, "Unauthorized"),
            (200, "Ok."),
            (200, r#"[{"hash":"a","name":"One","size":10,"completed":10,"ratio":2.0,"state":"uploading"}]"#),
        ])
        .await;

        let list = client_against(&base, "2.11.0").get_torrent_info().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].ratio_permille, 2000);

        let seen = server.await.unwrap();
        assert_eq!(seen.len(), 3);
        let first = text(&seen[0]);
        let replayed = text(&seen[2]);
        assert!(first.starts_with("GET /api/v2/torrents/info"), "{first}");
        assert_eq!(
            first.lines().next(),
            replayed.lines().next(),
            "the replay must be the same request"
        );
    }

    /// One retry, not a loop: a server that keeps saying 403 after a successful
    /// login is a problem to report, not to hammer.
    #[tokio::test]
    async fn a_second_403_is_reported_rather_than_looped() {
        let (base, server) =
            fake_api(vec![(403, "Unauthorized"), (200, "Ok."), (403, "Unauthorized")]).await;

        let err = client_against(&base, "2.11.0")
            .get_torrent_info()
            .await
            .expect_err("a persistent 403 must fail");
        assert!(err.to_string().contains("refused the session"), "{err}");

        assert_eq!(server.await.unwrap().len(), 3, "exactly one retry");
    }

    #[tokio::test]
    async fn the_add_is_multipart_with_a_part_named_torrents() {
        let (base, server) = fake_api(vec![(200, "Ok.")]).await;

        client_against(&base, "2.11.0")
            .add_torrent_and_start(&a_torrent_b64(), &named("Some Release"))
            .await
            .unwrap();

        let seen = server.await.unwrap();
        let request = text(&seen[0]);
        assert!(request.starts_with("POST /api/v2/torrents/add"), "{request}");
        assert!(request.contains("multipart/form-data; boundary="), "{request}");
        assert!(request.contains("name=\"torrents\""), "{request}");
        // The torrent itself must arrive intact, not re-encoded.
        let body = a_torrent();
        assert!(seen[0].windows(body.len()).any(|w| w == body), "torrent bytes missing");
    }

    /// qBittorrent 5.0 renamed the field. Sending the wrong one means the add
    /// silently honours the user's "add stopped" preference instead.
    #[tokio::test]
    async fn the_start_field_follows_the_api_version() {
        for (version, expected, unexpected) in
            [("2.9.3", "paused", "stopped"), ("2.11.0", "stopped", "paused")]
        {
            let (base, server) = fake_api(vec![(200, "Ok.")]).await;
            client_against(&base, version)
                .add_torrent_and_start(&a_torrent_b64(), &named("R"))
                .await
                .unwrap();

            // Multipart, so the field is a part header plus a body, not a
            // `key=value` pair.
            let request = text(&server.await.unwrap()[0]);
            assert!(
                request.contains(&format!("name=\"{expected}\"\r\n\r\nfalse")),
                "{version} should have sent {expected}=false: {request}"
            );
            assert!(
                !request.contains(&format!("name=\"{unexpected}\"")),
                "{version} must not send {unexpected}: {request}"
            );
        }
    }

    /// Both success shapes, because the published docs only describe the first
    /// and a real 5.2.1 answers with the second. Requiring `Ok.` reported every
    /// add against a 5.x server as failed *after* it had already succeeded.
    #[tokio::test]
    async fn either_generations_success_body_counts_as_success() {
        for body in [
            "Ok.",
            r#"{"added_torrent_ids":["e6904c6abb6c11b092c5c8e4f272140ecc1f4e9c"],"failed":[]}"#,
        ] {
            let (base, _server) = fake_api(vec![(200, body)]).await;
            client_against(&base, "2.11.0")
                .add_torrent_and_start(&a_torrent_b64(), &named("R"))
                .await
                .unwrap_or_else(|e| panic!("{body} should be success: {e}"));
        }
    }

    /// ...but a 2xx that says it added nothing is still a failure.
    #[tokio::test]
    async fn a_success_status_that_added_nothing_is_a_failure() {
        for body in [r#"{"added_torrent_ids":[]}"#, "Fails."] {
            let (base, _server) = fake_api(vec![(200, body)]).await;
            let err = client_against(&base, "2.11.0")
                .add_torrent_and_start(&a_torrent_b64(), &named("R"))
                .await
                .expect_err("{body} must not be treated as success");
            assert!(!err.to_string().is_empty());
        }
    }

    /// A duplicate is by far the most common 409, and qBittorrent's body for it
    /// is the literal word "Conflict", which explains nothing to the person
    /// reading the reply in an IRC channel.
    #[tokio::test]
    async fn a_409_names_the_likely_cause() {
        let (base, _server) = fake_api(vec![(409, "Conflict")]).await;

        let err = client_against(&base, "2.11.0")
            .add_torrent_and_start(&a_torrent_b64(), &named("R"))
            .await
            .expect_err("409 must be an error");

        assert!(err.to_string().contains("already added"), "{err}");
    }

    #[tokio::test]
    async fn a_415_is_reported_as_an_invalid_torrent_file() {
        let (base, _server) = fake_api(vec![(415, "")]).await;

        let err = client_against(&base, "2.11.0")
            .add_torrent_and_start(&a_torrent_b64(), &named("R"))
            .await
            .expect_err("415 must be an error");

        assert!(err.to_string().contains("not a valid torrent"), "{err}");
    }

    /// An HTML login page served by the tracker instead of a torrent is caught
    /// locally, naming the release, rather than as a bare 415 from qBittorrent.
    #[tokio::test]
    async fn a_file_that_is_not_a_torrent_is_caught_before_upload() {
        let (base, _server) = fake_api(vec![]).await;
        let html = general_purpose::STANDARD.encode("<html>Please log in</html>");

        let err = client_against(&base, "2.11.0")
            .add_torrent_and_start(&html, &named("Some Release"))
            .await
            .expect_err("not a torrent");

        assert!(err.to_string().contains("Some Release"), "{err}");
        assert!(err.to_string().contains("could not be parsed"), "{err}");
    }

    /// Retrying a rejected password is how a typo becomes an hour-long IP ban,
    /// so it has to be distinguishable from "not up yet".
    #[tokio::test]
    async fn bad_credentials_are_fatal_and_named() {
        let (base, _server) = fake_api(vec![(403, "Unauthorized"), (200, "Fails.")]).await;

        let err = QBittorrent::new(&opts(base, "user".into(), "wrong".into(), String::new()))
            .await
            .expect_err("bad credentials must fail");

        assert!(err.downcast_ref::<Unrecoverable>().is_some(), "must not be retried: {err}");
        assert!(err.to_string().contains("username or password"), "{err}");
    }

    #[tokio::test]
    async fn a_banned_ip_says_so() {
        let (base, _server) = fake_api(vec![(403, "Unauthorized"), (403, "banned")]).await;

        let err = QBittorrent::new(&opts(base, "user".into(), "pw".into(), String::new()))
            .await
            .expect_err("a ban must fail");

        assert!(err.downcast_ref::<Unrecoverable>().is_some());
        assert!(err.to_string().contains("banned"), "{err}");
    }

    /// Every one of these strings is printed verbatim into an IRC channel, a
    /// Telegram chat or a Slack channel by `TorrentOutcome::AddFailed`.
    #[tokio::test]
    async fn no_secret_or_url_reaches_a_user_visible_error() {
        // A dead port, so the transport itself fails -- the path where
        // reqwest's own Display would have embedded the URL.
        let dead = "http://127.0.0.1:1";
        let unreachable = client_against(dead, "2.11.0")
            .add_torrent_and_start(&a_torrent_b64(), &named("R"))
            .await
            .expect_err("nothing is listening");

        let (base, _server) = fake_api(vec![(200, "Ok."), (500, "boom")]).await;
        let http_error = client_against(&base, "2.11.0")
            .get_torrent_info()
            .await
            .expect_err("500 must fail");

        for err in [unreachable.to_string(), http_error.to_string()] {
            assert!(!err.contains("hunter2"), "password leaked: {err}");
            assert!(!err.contains("127.0.0.1"), "URL leaked: {err}");
            assert!(err.len() < 80, "too long for one IRC line: {err}");
        }
    }

    // -----------------------------------------------------------------------
    // Against a real qBittorrent
    // -----------------------------------------------------------------------

    /// Add a torrent to a live client and read it back.
    ///
    /// Ignored by default; point it at one with
    /// `QBITTORRENT_TEST_URL` / `_USER` / `_PASS`. Follows the
    /// `RTORRENT_TEST_SOCKET` convention rather than the LAN address hardcoded
    /// in flood.rs, which only ever worked on one machine.
    ///
    /// The fixture's tracker is a dead loopback UDP port, so adding it announces
    /// to nobody.
    #[tokio::test]
    #[ignore = "requires a running qBittorrent"]
    async fn a_real_client_accepts_and_reports_a_torrent() {
        let url = std::env::var("QBITTORRENT_TEST_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
        let user = std::env::var("QBITTORRENT_TEST_USER").unwrap_or_default();
        let pass = std::env::var("QBITTORRENT_TEST_PASS").unwrap_or_default();

        let qbt = QBittorrent::new(&opts(url, user, pass, "irc2torrent-test".into()))
            .await
            .expect("could not reach qBittorrent");

        // Start from a known state. Without this the second run adds a
        // duplicate, qBittorrent answers 409, and the test only ever passes
        // once -- which is how this was written the first time.
        remove_test_torrent(&qbt).await;

        qbt.add_torrent_and_start(&a_torrent_b64(), &named("test"))
            .await
            .expect("add failed");

        // Adding is asynchronous on qBittorrent's side; give it a bounded
        // moment to show up rather than a fixed sleep.
        let mut found = None;
        for _ in 0..20 {
            if let Some(t) = qbt.torrents().await.unwrap().into_iter().find(|t| t.name == "test") {
                found = Some(t);
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let found = found.expect("the torrent never appeared in the list");
        remove_test_torrent(&qbt).await;

        assert_eq!(found.name, "test");
        // The mapping, against a real server rather than a canned body.
        assert_eq!(found.to_info().name, "test");
    }

    async fn remove_test_torrent(qbt: &QBittorrent) {
        let Ok(list) = qbt.torrents().await else { return };
        for t in list.into_iter().filter(|t| t.name == "test") {
            let hash = t.hash.clone();
            let _ = qbt
                .send_authed("torrents/delete", || {
                    qbt.client
                        .post(qbt.endpoint("torrents/delete"))
                        .form(&[("hashes", hash.as_str()), ("deleteFiles", "true")])
                })
                .await;
        }
    }
}
