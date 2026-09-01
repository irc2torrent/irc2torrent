use std::any::Any;
use std::collections::HashMap;
use std::fmt::Debug;
use std::future::Future;
use std::path::PathBuf;

use anyhow::Error;
use base64::Engine;
use base64::engine::general_purpose;
use chrono::{DateTime, Local, NaiveDateTime, offset, Utc};
use futures::StreamExt;
use lava_torrent::torrent::v1::Torrent;
use log::{error, info};

use dxr::{DxrError, TryFromValue, Value};
use dxr_client::{Call, Client, ClientBuilder, ClientError, Url};

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

use crate::announce::Announce;
use crate::config::config::rTorrentOptions;
use crate::template::TextTemplate;
use crate::clients::{CompletionRow, DownloadResult, TorrentInfo};

pub struct rTorrent {
    client: Client,
    /// Compiled once. `clients` needs a restart to change, so this inherits
    /// that documented behaviour.
    tags_template: Option<TextTemplate>,
}

/// An integer from rTorrent, whichever width its XML-RPC backend chose to send.
///
/// The two backends disagree, and dxr is strict in both directions:
///
/// * `--with-xmlrpc-tinyxml2` (what this image builds) serialises *every*
///   integer as `<i8>`. There is no `<i4>` path in it at all -- see the
///   `OpenElement("i8", ...)` calls in rTorrent's `src/rpc/xmlrpc_tinyxml2.cc`.
/// * `--with-xmlrpc-c` emits `<i4>` for anything that fits in 32 bits.
///
/// dxr's `i32` accepts only `<i4>` and its `i64` only `<i8>`, so naming either
/// concrete type works against exactly one of the two builds and fails against
/// the other with `WrongType`. Asking for this instead works against both.
///
/// This is what made every add fail after the move to rakshasa + tinyxml2:
/// `load.raw_start_verbose` was typed `i32`, so the call errored on a *successful*
/// load and the torrent looked rejected when rTorrent had in fact taken it.
#[derive(Debug, PartialEq)]
struct RpcInt(i64);

impl TryFromValue for RpcInt {
    fn try_from_value(value: &Value) -> Result<Self, DxrError> {
        if let Ok(wide) = i64::try_from_value(value) {
            return Ok(RpcInt(wide));
        }
        // Report the i4 attempt's error, which carries the type actually seen.
        i32::try_from_value(value).map(|narrow| RpcInt(narrow.into()))
    }
}

impl TryFromValue for DownloadResult {
    fn try_from_value(value: &Value) -> Result<Self, DxrError> {
        if let Ok(arr) = Vec::<Value>::try_from_value(value) {
            if arr.len() != 3 {
                return Err(DxrError::invalid_data("Expected array of length 3".to_owned()));
            }
            let mut name = "".to_string();
            let mut size = 0;
            let mut creation_date = 0;
            if let Ok(n) = String::try_from_value(&arr[0]) {
                name = n.clone();
            }
            if let Ok(s) = i64::try_from_value(&arr[1]) {
                size = s;
            }
            if let Ok(cd) = i64::try_from_value(&arr[2]) {
                //convert from unix epoch seconds to datetime
                creation_date = cd;
            }
            Ok(DownloadResult {
                name,
                size,
                creation_date,
            })
        } else {
            Err(DxrError::invalid_data("Expected an array".to_owned()))
        }
    }
}

/// Decodes one row of `d.multicall2` asking for hash, name and completion.
///
/// `CompletionRow` itself now lives in `clients::mod`, since qBittorrent
/// produces it too; only this decoder is rTorrent-shaped.
impl TryFromValue for CompletionRow {
    fn try_from_value(value: &Value) -> Result<Self, DxrError> {
        let row = Vec::<Value>::try_from_value(value)?;
        if row.len() != 3 {
            return Err(DxrError::invalid_data(format!(
                "Expected [hash, name, complete], got {} fields",
                row.len()
            )));
        }
        Ok(CompletionRow {
            hash: String::try_from_value(&row[0])?,
            name: String::try_from_value(&row[1])?,
            // d.complete is 0 or 1, and arrives as <i8> from the tinyxml2
            // backend or <i4> from xmlrpc-c -- hence RpcInt rather than either
            // concrete width. See its doc comment.
            complete: RpcInt::try_from_value(&row[2])?.0 != 0,
        })
    }
}

impl TryFromValue for TorrentInfo {
    fn try_from_value(value: &Value) -> Result<Self, DxrError> {
        let row = Vec::<Value>::try_from_value(value)?;
        if row.len() != 4 {
            return Err(DxrError::invalid_data(format!(
                "Expected [name, size, completed, ratio], got {} fields",
                row.len()
            )));
        }
        // RpcInt throughout: the tinyxml2 backend sends <i8> and xmlrpc-c <i4>.
        Ok(TorrentInfo {
            name: String::try_from_value(&row[0])?,
            size_bytes: RpcInt::try_from_value(&row[1])?.0,
            completed_bytes: RpcInt::try_from_value(&row[2])?.0,
            ratio_permille: RpcInt::try_from_value(&row[3])?.0,
        })
    }
}

impl rTorrent {
    /// Every torrent with enough detail to be worth a line each.
    pub(crate) async fn get_torrent_info(&self) -> Result<Vec<TorrentInfo>, Error> {
        let request = Call::new(
            "d.multicall2",
            ("", "main", "d.name=", "d.size_bytes=", "d.completed_bytes=", "d.ratio="),
        );
        self.client
            .call::<_, Vec<TorrentInfo>>(request)
            .await
            .map_err(|e| Error::msg(format!("Could not list torrents: {e:?}")))
    }

    /// Every torrent's hash, name and whether it has finished.
    pub(crate) async fn get_completion(&self) -> Result<Vec<CompletionRow>, Error> {
        let request =
            Call::new("d.multicall2", ("", "main", "d.hash=", "d.name=", "d.complete="));
        self.client
            .call::<_, Vec<CompletionRow>>(request)
            .await
            .map_err(|e| Error::msg(format!("Could not list torrents: {e:?}")))
    }

    pub(crate) async fn get_dl_list(&self) -> Result<Vec<DownloadResult>, Error> {
        let request = Call::new("d.multicall2", ("", "main", "d.name=", "d.size_bytes=", "d.creation_date="));
        let result: Result<Vec<DownloadResult>, ClientError> = self.client.call(request).await;
        match result {
            Ok(r) => {
                info!("Torrent load result: {r:?}");
                Ok(r)
            }
            Err(e) => {
                error!("Error loading torrent: {:?}", e);
                Err(Error::msg(format!("Error loading torrent: {:?}", e)))
            }
        }
    }

    pub(crate) async fn add_torrent_and_start(
        &self,
        file: &str,
        announce: &Announce,
    ) -> Result<(), Error> {
        let name = announce.name.clone();
        if let Ok(bytes) = &general_purpose::STANDARD.decode(file.as_bytes()) {
            let request = Call::new("load.raw_start_verbose", ("", bytes.as_slice()));
            // let request = dxr::client::Call::new("load.raw_start_verbose", file);
            let result: Result<RpcInt, ClientError> = self.client.call(request).await;

            // The bytes came from the tracker, so they may not be a well-formed
            // torrent at all -- an error page or a truncated download is enough.
            // This used to be `.unwrap()`, which panicked the whole process
            // *after* the torrent had already been handed to rTorrent.
            let the_hash = match Torrent::read_from_bytes(bytes) {
                Ok(t) => t.info_hash(),
                Err(e) => {
                    error!("Could not parse torrent ({name}) to compute its hash: {e:?}");
                    return Err(Error::msg(format!(
                        "Torrent ({name}) could not be parsed: {e}"
                    )));
                }
            };

            match result {
                Ok(r) => {
                    info!("Torrent load result: ({name}) {r:?}");

                    // Stamp the "added" time ourselves.
                    //
                    // rTorrent keeps no added-at timestamp of its own; Flood and
                    // ruTorrent both read the `addtime` custom field, and when it
                    // is missing or zero the column renders empty.
                    //
                    // This used to call a user-defined `fix_addtime`, which had to
                    // be declared in .rtorrent.rc as
                    //   d.custom.set=addtime,(cat,$d.creation_date=)
                    // and is wrong twice over: it records the torrent's *creation*
                    // date rather than when it was added, and `d.creation_date` is
                    // 0 for any .torrent carrying no `creation date` key -- so the
                    // field was set to a literal "0" and Flood showed nothing.
                    // Confirmed against rTorrent 0.16.20: for such a torrent
                    // creation_date is 0, fix_addtime succeeds, and addtime is "0".
                    //
                    // Setting d.custom directly also removes the requirement to
                    // declare anything in .rtorrent.rc at all.
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let stamp =
                        Call::new("d.custom.set", (the_hash.clone(), "addtime", now.to_string()));

                    match self.client.call::<_, RpcInt>(stamp).await {
                        Ok(_) => {
                            info!("Added time set for {name} ({the_hash}).");
                        }
                        Err(e) => {
                            // The torrent is loaded and downloading by now, so a
                            // missing timestamp is cosmetic and must not be
                            // reported as a failed add.
                            error!("Could not set the added time for {name}: {e:?}");
                        }
                    }

                    // Tags, into the same custom-field mechanism. rTorrent has
                    // no category of its own: `d.custom1` is the single label
                    // field, and Flood asks for it in its ordinary torrent-list
                    // call, so this shows up with nothing else configured.
                    if let Some(template) = &self.tags_template {
                        let rendered = template.render(announce);
                        let encoded = encode_tags(rendered.split(','));
                        if !encoded.is_empty() {
                            let tag = Call::new(
                                "d.custom.set",
                                (the_hash.clone(), "custom1", encoded.clone()),
                            );
                            match self.client.call::<_, RpcInt>(tag).await {
                                Ok(_) => info!("Tags set for {name} ({the_hash}): {encoded}"),
                                // Non-fatal for the same reason as above: the
                                // torrent is already loaded, and reporting a
                                // failed add would be a lie.
                                Err(e) => error!("Could not set tags for {name}: {e:?}"),
                            }
                        }
                    }

                    Ok(())
                }
                Err(e) => {
                    error!("File upload err: ({}) {:?}", name, e);
                    Err(Error::msg(format!("File upload err: ({}) {:?}", name, e)))
                },
            }
        } else {
            Err(Error::msg("Failed to decode file"))
        }
    }
    async fn check_config(&self) -> Result<(), Error> {
        match self.get_dl_list().await { Ok(_) => Ok(()), Err(e) => Err(e) }
    }
}

impl rTorrent {
    pub async fn new(options: &rTorrentOptions) -> Result<rTorrent, Error> {
        let url = Url::parse(&options.xmlrpc_url)?;
        let client = ClientBuilder::new(url)
            .user_agent("dxr-client-example")
            // .basic_auth(username, Some(password))
            .build();

        // Parsed here so a malformed template is a startup error rather than a
        // surprise on the first add, matching the qBittorrent client.
        let tags_template = if options.tags_template.trim().is_empty() {
            None
        } else {
            Some(TextTemplate::parse(
                &options.tags_template,
                "[clients.rTorrent] tags_template",
            )?)
        };

        Ok(Self { client, tags_template })
    }
}

/// Everything `encodeURIComponent` leaves alone: `A-Za-z0-9 - _ . ! ~ * ' ( )`.
///
/// Matching it exactly is not required -- `decodeURIComponent` unescapes
/// whatever it is given, so encoding *more* would still round-trip -- but
/// matching keeps `d.custom1` byte-identical to what Flood itself writes, which
/// is what makes a tag set here indistinguishable from one set in the UI.
const JS_URI_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

/// Encode tags the way Flood does, so it can read them back.
///
/// Flood's contract, from `server/services/rTorrent/util/torrentPropertiesUtil.ts`
/// and `constants/methodCallConfigs/torrentList.ts`: trim each tag,
/// `encodeURIComponent` it, drop empties and duplicates, join with `,`. Reading
/// is the mirror -- split on `,`, `decodeURIComponent` each element -- which is
/// why the comma must be encoded inside a tag rather than left to be read as a
/// separator.
///
/// The same convention ruTorrent uses, and Flood asks for `d.custom1` in its
/// ordinary torrent-list call, so nothing has to be configured to see these.
fn encode_tags<'a>(tags: impl IntoIterator<Item = &'a str>) -> String {
    let mut seen: Vec<String> = Vec::new();
    for tag in tags {
        let encoded = utf8_percent_encode(tag.trim(), JS_URI_COMPONENT).to_string();
        if !encoded.is_empty() && !seen.contains(&encoded) {
            seen.push(encoded);
        }
    }
    seen.join(",")
}

/// The tag encoder, tested on its own: the module below does
/// `use tokio::test`, which shadows `#[test]` and would make these async
/// for no reason.
#[cfg(test)]
mod encode_test {
    use super::encode_tags;


        /// Flood splits `d.custom1` on `,` and `decodeURIComponent`s each element
        /// (`constants/methodCallConfigs/torrentList.ts`), so a comma inside a tag
        /// has to be encoded or it reads back as two tags.
        #[test]
        fn tags_are_encoded_the_way_flood_reads_them() {
            assert_eq!(encode_tags(["Movies", "4K"]), "Movies,4K");
            assert_eq!(encode_tags(["Movies :: 4K"]), "Movies%20%3A%3A%204K");
            assert_eq!(encode_tags(["a,b"]), "a%2Cb", "a comma must not become a separator");
        }

        #[test]
        fn tags_are_trimmed_deduplicated_and_emptied_out() {
            // Matching encodeTags in torrentPropertiesUtil.ts, which trims, drops
            // empties and skips duplicates before joining.
            assert_eq!(encode_tags([" hd ", "hd", "", "  ", "remux"]), "hd,remux");
            assert_eq!(encode_tags([""]), "");
        }

        /// The set `encodeURIComponent` leaves alone. Encoding more would still
        /// round-trip, but matching keeps our `d.custom1` byte-identical to one
        /// Flood wrote itself.
        #[test]
        fn the_unreserved_set_matches_encodeuricomponent() {
            let unreserved = "AZaz09-_.!~*'()";
            assert_eq!(encode_tags([unreserved]), unreserved);
            // And the things it does escape, which a release name is full of.
            assert_eq!(encode_tags(["a b"]), "a%20b");
            assert_eq!(encode_tags(["a/b"]), "a%2Fb");
        }
}

//tests
#[cfg(test)]
pub mod test {
    use crate::announce::Announce;
    use crate::config::config::rTorrentOptions;

    /// Options pointing at the live test socket, with no tags configured.
    fn sock_options() -> rTorrentOptions {
        rTorrentOptions {
            xmlrpc_url: format!("unix:{}", get_sock_addr()),
            tags_template: String::new(),
        }
    }

    use std::fs;
    use std::os::unix::prelude::{FileTypeExt, PermissionsExt};
    use tokio::test;
    use crate::clients::rtorrent::{rTorrent, RpcInt};
    use dxr::{TryFromValue, Value};

    /// rTorrent's tinyxml2 backend answers `<i8>` and its xmlrpc-c backend
    /// `<i4>`; both must parse. Typing these calls `i32` is what made every add
    /// fail with `WrongType { argument: "i8", expected: "i4" }` on a load that
    /// had actually succeeded.
    #[test]
    async fn an_rpc_int_accepts_either_width() {
        assert_eq!(RpcInt::try_from_value(&Value::i8(42)).unwrap(), RpcInt(42));
        assert_eq!(RpcInt::try_from_value(&Value::i4(42)).unwrap(), RpcInt(42));

        // A value that only fits in 64 bits must not be truncated.
        assert_eq!(RpcInt::try_from_value(&Value::i8(i64::MAX)).unwrap(), RpcInt(i64::MAX));
    }

    #[test]
    async fn an_rpc_int_still_rejects_a_non_integer() {
        assert!(RpcInt::try_from_value(&Value::string("not a number".to_string())).is_err());
    }

    /// End-to-end against a live rTorrent: proves RpcInt parses what the running
    /// build actually puts on the wire, rather than what we believe it sends.
    ///
    /// `system.pid` is used because it returns a small integer and touches no
    /// tracker -- unlike the upload test below, which announces for real.
    ///
    /// Run with `RTORRENT_TEST_SOCKET=... cargo test -- --ignored`.
    #[ignore = "requires a running rTorrent"]
    #[tokio::test]
    pub async fn an_rpc_int_parses_a_live_rtorrent_integer() {
        use dxr_client::Call;

        let rt = rTorrent::new(&sock_options()).await.unwrap();
        let pid: RpcInt = rt.client.call(Call::new("system.pid", ())).await.unwrap();
        assert!(pid.0 > 0, "system.pid should be positive, got {pid:?}");

        // The same call typed i32 is what shipped, and must still fail here --
        // otherwise this test would pass for the wrong reason on a build whose
        // backend does emit <i4>.
        let narrow: Result<i32, _> = rt.client.call(Call::new("system.pid", ())).await;
        assert!(narrow.is_err(), "expected <i8> from the tinyxml2 backend");
    }

    // Integration test: needs a live rTorrent SCGI socket. Run explicitly with
    // `cargo test -- --ignored` against a real instance.
    #[ignore = "requires a running rTorrent"]
    #[tokio::test]
    pub async fn test_torrent_upload() {
        // let current_path = std::env::current_dir().unwrap();
        let path = get_sock_addr();
        if check_socket_file(&path) {
            println!("The socket file is accessible and permissions are correct.");
        } else {
            panic!("The socket file is not accessible or permissions are incorrect.");
        }
        let mut rt = rTorrent::new(&sock_options()).await.unwrap();
        let announce = Announce::new(
            "Diners.Drive-Ins.and.Dives.S48E04.1080p.WEB.h264-FREQUENCY".to_string(),
            "1".to_string(),
        );
        let r = rt.add_torrent_and_start("ZDg6YW5ub3VuY2U3NjpodHRwczovL3RyYWNrZXIudG9ycmVudGxlZWNoLm9yZy9hL2ZmZGJhZGJmZjk1OWMwYjkwZjY4NTA4ZWFiNGQwZTY5L2Fubm91bmNlMTM6YW5ub3VuY2UtbGlzdGxsNzY6aHR0cHM6Ly90cmFja2VyLnRvcnJlbnRsZWVjaC5vcmcvYS9mZmRiYWRiZmY5NTljMGI5MGY2ODUwOGVhYjRkMGU2OS9hbm5vdW5jZTc2Omh0dHBzOi8vdHJhY2tlci50bGVlY2hyZWxvYWQub3JnL2EvZmZkYmFkYmZmOTU5YzBiOTBmNjg1MDhlYWI0ZDBlNjkvYW5ub3VuY2VlZTEwOmNyZWF0ZWQgYnkxMzpta3RvcnJlbnQgMS4xMTM6Y3JlYXRpb24gZGF0ZWkxNzA2MzIzNTg1ZTQ6aW5mb2Q1OmZpbGVzbGQ2Omxlbmd0aGkxMjU5MDg3MDY0ZTQ6cGF0aGw2MjpEaW5lcnMuRHJpdmUtSW5zLmFuZC5EaXZlcy5TNDhFMDQuMTA4MHAuV0VCLmgyNjQtRlJFUVVFTkNZLm1rdmVlZDY6bGVuZ3RoaTYzODI1NTExZTQ6cGF0aGw2OlNhbXBsZTY5OnNhbXBsZS1kaW5lcnMuZHJpdmUtaW5zLmFuZC5kaXZlcy5zNDhlMDQuMTA4MHAud2ViLmgyNjQtZnJlcXVlbmN5Lm1rdmVlZDY6bGVuZ3RoaTcyZTQ6cGF0aGw2MjpkaW5lcnMuZHJpdmUtaW5zLmFuZC5kaXZlcy5zNDhlMDQuMTA4MHAud2ViLmgyNjQtZnJlcXVlbmN5Lm5mb2VlZDY6bGVuZ3RoaTcyMjFlNDpwYXRobDYyOmRpbmVycy5kcml2ZS1pbnMuYW5kLmRpdmVzLnM0OGUwNC4xMDgwcC53ZWIuaDI2NC1mcmVxdWVuY3kuc3JyZWVlNDpuYW1lNTg6RGluZXJzLkRyaXZlLUlucy5hbmQuRGl2ZXMuUzQ4RTA0LjEwODBwLldFQi5oMjY0LUZSRVFVRU5DWTEyOnBpZWNlIGxlbmd0aGkxMDQ4NTc2ZTY6cGllY2VzMjUyNDA6jc8Kq9o3sIaLwS0seAHETKwXm69In1gVw0ZAq1kt3RqQ1sb+B9MZGeD7Rr0hNtcqrjGSZtKEOyb5bjHZENkfqZ/dqrdPACryGAPz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", &announce);
        r.await;
    }

    #[ignore = "requires a running rTorrent"]
    #[tokio::test]
    pub async fn test_dl_list() {
        let path = get_sock_addr();
        if check_socket_file(&path) {
            println!("The socket file is accessible and permissions are correct.");
        } else {
            panic!("The socket file is not accessible or permissions are incorrect.");
        }
        let mut rt = rTorrent::new(&sock_options()).await.unwrap();
        let r = rt.get_dl_list().await;
        println!("Download List: {:?}", r);
    }

    /// Socket for the `--ignored` integration tests.
    ///
    /// Was hard-coded to one developer's machine, which made every test here
    /// unrunnable by anyone else -- including in the container, where the whole
    /// stack is actually available.
    fn get_sock_addr() -> String {
        std::env::var("RTORRENT_TEST_SOCKET")
            .unwrap_or_else(|_| "/config/.local/share/rtorrent/rtorrent.sock".to_string())
    }

    fn check_socket_file(path: &str) -> bool {
        match fs::metadata(path) {
            Ok(metadata) => {
                let permissions = metadata.permissions();
                let mode = permissions.mode();
                // Check if the file is a socket and it's readable and writable
                metadata.file_type().is_socket() && (mode & 0o600) == 0o600
            }
            Err(_) => false,
        }
    }
}