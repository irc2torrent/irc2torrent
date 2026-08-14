use std::path::PathBuf;
use anyhow::Error;

use crate::announce::Announce;

pub mod http;
pub(crate) mod url_template;

pub trait TorrentPlatform {
    fn get_torrent_files_dir(&self) -> &PathBuf;
    async fn download_torrent(&self, announce: &Announce) -> Result<String, Error>;
}
