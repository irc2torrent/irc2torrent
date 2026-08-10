use std::fmt::Debug;
use std::future::Future;

use base64::Engine;
use chrono::{DateTime, Local};

pub mod flood;
pub mod qbittorrent;
pub mod rtorrent;

pub enum TorrentClientsEnum {
    Rtorrent(rtorrent::rTorrent),
    Flood(flood::Flood),
    QBittorrent(qbittorrent::QBittorrent),
}

/// One row of "which torrents are finished", from whichever client.
///
/// Keyed by hash, not name: two torrents can carry the same name, and a name is
/// not a stable identity across a restart.
///
/// Lives here rather than in `rtorrent` because two clients now produce it. The
/// dxr `TryFromValue` decoder stays over there, since only rTorrent needs it.
#[derive(Debug, Clone)]
pub struct CompletionRow {
    pub hash: String,
    pub name: String,
    pub complete: bool,
}

/// An error that retrying cannot fix.
///
/// Distinguished so the startup retry loop stops after the first attempt rather
/// than hammering a client that has already said no. qBittorrent bans an IP for
/// an hour after five failed logins, and the loop's ten attempts in thirty
/// seconds would reach that with room to spare -- turning a typo in the password
/// into an hour of lockout.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Unrecoverable(pub String);

#[derive(serde::Deserialize, serde::Serialize, Clone, PartialEq, Eq, Hash, Default)]
pub struct DownloadResult {
    name: String,
    size: i64,
    creation_date: i64,
}

/// A torrent as `torrentlist` reports it.
///
/// Richer than `DownloadResult`, which carries the creation date but says
/// nothing about progress -- the thing you actually want when asking a client
/// what it is doing.
#[derive(Debug, Clone, PartialEq)]
pub struct TorrentInfo {
    pub name: String,
    pub size_bytes: i64,
    pub completed_bytes: i64,
    /// rTorrent reports the ratio per mille, so 1500 means 1.50.
    pub ratio_permille: i64,
}

impl TorrentInfo {
    pub fn percent_done(&self) -> u64 {
        if self.size_bytes <= 0 {
            // A magnet whose metadata has not arrived yet has no size, and
            // dividing by it would panic.
            return 0;
        }
        ((self.completed_bytes.max(0) as u128 * 100) / self.size_bytes as u128).min(100) as u64
    }

    pub fn is_complete(&self) -> bool {
        self.size_bytes > 0 && self.completed_bytes >= self.size_bytes
    }

    pub fn readable_size(&self) -> String {
        human_bytes(self.size_bytes)
    }

    pub fn ratio(&self) -> f64 {
        self.ratio_permille as f64 / 1000.0
    }

    /// One line for an IRC reply.
    pub fn summary(&self) -> String {
        if self.is_complete() {
            format!("{} — {}, done, ratio {:.2}", self.name, self.readable_size(), self.ratio())
        } else {
            format!(
                "{} — {}, {}%, ratio {:.2}",
                self.name,
                self.readable_size(),
                self.percent_done(),
                self.ratio()
            )
        }
    }
}

fn human_bytes(bytes: i64) -> String {
    let mut size = bytes.max(0) as f64;
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut i = 0;
    while size >= 1024.0 && i + 1 < units.len() {
        size /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{}{}", size as i64, units[i])
    } else {
        format!("{size:.2}{}", units[i])
    }
}

impl DownloadResult {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `creation_date` comes from the torrent's own metadata, so it is attacker
    /// controlled. `from_timestamp` returns None outside roughly +/-262,000 years,
    /// and the previous `.unwrap()` turned any such torrent into a process-wide
    /// panic the moment the download list was formatted. Fall back to the epoch.
    pub fn get_utc_creation_date(&self) -> DateTime<Local> {
        let ts = DateTime::from_timestamp(self.creation_date, 0)
            .unwrap_or_else(|| DateTime::from_timestamp(0, 0).expect("epoch is always valid"));
        DateTime::from(ts)
    }
    pub fn get_readable_size(&self) -> String {
        let mut size = self.size as f64;
        let units = ["B", "KB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];
        let mut i = 0;
        while size > 1024.0 {
            i += 1;
            size = size / 1024.0;
        }
        format!("{:.2}{}", size, units[i])
    }
}

impl ToString for DownloadResult {
    fn to_string(&self) -> String {
        format!("Name: {}, Size: {}, Creation Date: {}", self.name, self.get_readable_size(), self.get_utc_creation_date())
    }
}

impl Debug for DownloadResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Name: {}, Size: {}, Creation Date: {}", self.name, self.get_readable_size(), self.get_utc_creation_date())
    }
}