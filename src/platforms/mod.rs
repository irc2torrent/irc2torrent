use std::path::PathBuf;
use anyhow::Error;

pub mod http;
pub(crate) mod url_template;

pub trait TorrentPlatform {
    fn get_torrent_files_dir(&self) -> &PathBuf;
    async fn download_torrent(&self, name: String, id: String) -> Result<String, Error>;
}
