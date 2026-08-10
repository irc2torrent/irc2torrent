use std::path::{Path, PathBuf};

use anyhow::{Context, Error};
use base64::engine::general_purpose;
use base64::Engine;
use log::info;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::config::config::PlatformOptions;
use crate::platforms::url_template::{Fields, UrlTemplate};
use crate::platforms::TorrentPlatform;

/// Longest filename we will produce, leaving room for the ".torrent" suffix on
/// filesystems with the usual 255-byte limit.
const MAX_STEM_LEN: usize = 200;

/// Any tracker that serves .torrent files over http(s).
///
/// Nothing tracker-specific is left in here: the URL comes from the user's
/// `download_url_template` and the id it carries came from the user's
/// `regex_for_announce_match`. Adding a network is configuration, not code.
pub(crate) struct HttpTracker {
    label: String,
    template: UrlTemplate,
    rss_key: String,
    torrent_dir: PathBuf,
}

impl HttpTracker {
    pub fn new(label: &str, options: &PlatformOptions) -> Result<Self, Error> {
        Ok(Self {
            label: label.to_string(),
            template: UrlTemplate::parse(&options.download_url_template)?,
            rss_key: options.rss_key.clone(),
            torrent_dir: PathBuf::from(&options.torrent_dir),
        })
    }
}

/// Turn an announce name into a filename that cannot escape the torrent directory.
///
/// The name arrives verbatim from an IRC message, so it is untrusted. Two traps
/// this guards against:
///   * `PathBuf::join` *replaces* the base when given an absolute path, so a name
///     of "/etc/cron.d/x" would otherwise be written outside the torrent dir;
///   * ".." segments traverse upward.
///
/// Rather than trying to strip dangerous sequences, keep only characters that are
/// unambiguously safe in a filename and collapse everything else. The result can
/// never contain a separator, so it cannot denote anything but a direct child.
fn sanitize_torrent_filename(name: &str) -> String {
    let mut stem: String = name
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '.',
        })
        .collect();

    // A leading dot would make a hidden file, and "." / ".." are special.
    while stem.starts_with('.') {
        stem.remove(0);
    }
    while stem.ends_with('.') {
        stem.pop();
    }

    if stem.len() > MAX_STEM_LEN {
        stem.truncate(MAX_STEM_LEN);
    }

    if stem.is_empty() {
        stem.push_str("torrent");
    }

    stem.push_str(".torrent");
    stem
}

/// Confirm `candidate` really is a direct child of `dir`.
///
/// `sanitize_torrent_filename` already makes traversal impossible, but this is
/// cheap and keeps the guarantee local to the write site: if the sanitiser is
/// ever loosened, this still refuses to write outside the directory.
fn assert_within(dir: &Path, candidate: &Path) -> Result<(), Error> {
    if candidate.parent() != Some(dir) {
        return Err(Error::msg(format!(
            "refusing to write outside the torrent directory: {}",
            candidate.display()
        )));
    }
    Ok(())
}

impl TorrentPlatform for HttpTracker {
    fn get_torrent_files_dir(&self) -> &PathBuf {
        &self.torrent_dir
    }

    async fn download_torrent(&self, name: String, id: String) -> Result<String, Error> {
        info!("Downloading torrent from {}: {}", self.label, name);

        let torrent_file = sanitize_torrent_filename(&name);

        // The rss_key is a secret and the template is user-supplied: never
        // include the URL in an error or log line. `render` percent-encodes each
        // substituted value and re-checks the host, because `name` and `id` come
        // straight off IRC -- see platforms::url_template.
        let url = self.template.render(Fields {
            id: &id,
            name: &name,
            file: &torrent_file,
            key: &self.rss_key,
        })?;

        let resp = reqwest::get(url)
            .await
            .map_err(|e| Error::msg(format!("Torrent request failed: {}", e.without_url())))?;

        if !resp.status().is_success() {
            return Err(Error::msg(format!(
                "Torrent request returned HTTP {}",
                resp.status()
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Error::msg(format!("Reading torrent body failed: {}", e.without_url())))?;

        let dir = self.get_torrent_files_dir();
        if !dir.exists() {
            fs::create_dir_all(dir)
                .await
                .with_context(|| format!("Creating torrent directory {}", dir.display()))?;
        }

        let out_path = dir.join(&torrent_file);
        assert_within(dir, &out_path)?;

        // Errors here used to be discarded, which reported truncated files as success.
        let mut out = fs::File::create(&out_path)
            .await
            .with_context(|| format!("Creating {}", out_path.display()))?;
        out.write_all(bytes.as_ref())
            .await
            .with_context(|| format!("Writing {}", out_path.display()))?;
        out.flush()
            .await
            .with_context(|| format!("Flushing {}", out_path.display()))?;

        Ok(general_purpose::STANDARD.encode(bytes.as_ref()))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn absolute_paths_cannot_escape() {
        let f = sanitize_torrent_filename("/etc/cron.d/pwn");
        assert!(!f.contains('/'), "{f}");
        assert_eq!(Path::new("/tmp").join(&f).parent(), Some(Path::new("/tmp")));
    }

    #[test]
    fn parent_traversal_cannot_escape() {
        for name in ["../../root/.ssh/authorized_keys", "..", "../..", r"..\..\windows"] {
            let f = sanitize_torrent_filename(name);
            assert!(!f.contains('/') && !f.contains('\\'), "{f}");
            assert_eq!(Path::new("/tmp").join(&f).parent(), Some(Path::new("/tmp")));
        }
    }

    #[test]
    fn empty_and_dot_only_names_get_a_fallback() {
        assert_eq!(sanitize_torrent_filename(""), "torrent.torrent");
        assert_eq!(sanitize_torrent_filename("..."), "torrent.torrent");
        assert_eq!(sanitize_torrent_filename("///"), "torrent.torrent");
    }

    #[test]
    fn long_names_are_capped() {
        let f = sanitize_torrent_filename(&"a".repeat(5000));
        assert!(f.len() <= MAX_STEM_LEN + ".torrent".len());
    }

    #[test]
    fn ordinary_names_survive_recognisably() {
        let f = sanitize_torrent_filename("Some Release S01 1080p WEB-DL");
        assert_eq!(f, "Some.Release.S01.1080p.WEB-DL.torrent");
    }

    #[test]
    fn assert_within_rejects_a_parent_escape() {
        assert!(assert_within(Path::new("/tmp"), Path::new("/tmp/a.torrent")).is_ok());
        assert!(assert_within(Path::new("/tmp"), Path::new("/etc/a.torrent")).is_err());
        assert!(assert_within(Path::new("/tmp"), Path::new("/tmp/sub/a.torrent")).is_err());
    }

    fn options(template: &str) -> PlatformOptions {
        PlatformOptions {
            download_url_template: template.to_string(),
            rss_key: "KEY".to_string(),
            torrent_dir: "/tmp".to_string(),
        }
    }

    /// `{file}` must be exactly what lands on disk, or the cached filename and
    /// the last URL segment drift apart and neither is obviously wrong.
    #[test]
    fn the_file_placeholder_matches_what_is_written() {
        let t = HttpTracker::new(
            "Example",
            &options("https://tracker.example.org/dl/{id}/{file}"),
        )
        .unwrap();
        let name = "Some Release S01 1080p WEB-DL";
        let url = t
            .template
            .render(Fields {
                id: "1",
                name,
                file: &sanitize_torrent_filename(name),
                key: "KEY",
            })
            .unwrap();
        assert!(url.ends_with("/Some.Release.S01.1080p.WEB-DL.torrent"), "{url}");
    }

    #[test]
    fn new_rejects_a_template_it_cannot_parse() {
        assert!(HttpTracker::new("Example", &options("")).is_err());
        assert!(HttpTracker::new("Example", &options("not a url")).is_err());
        assert!(HttpTracker::new("Example", &options("https://h.example/{nope}")).is_err());
    }
}
