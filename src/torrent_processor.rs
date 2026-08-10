pub mod torrent {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    use anyhow::Error;
    use base64;
    use base64::Engine as _;
    use log::{error, info};
    use pub_sub::{PubSub, Subscription};
    use regex::Regex;

    use crate::clients::{DownloadResult, TorrentClientsEnum, TorrentInfo};
    use crate::config::config::Config;
    use crate::notify::{Event, Notifier};
    use crate::platforms::http::HttpTracker;
    use crate::platforms::TorrentPlatform;

    pub struct TorrentProcessor {
        evt_channel: PubSub<String>,
        subs_cfg: Vec<Subscription<String>>,
        torrent_client: TorrentClientsEnum,
        torrent_platform: HttpTracker,
        options: Rc<RefCell<Config>>,
        /// Null unless notifications are configured, so call sites below stay
        /// unconditional.
        notifier: Notifier,
        // dl_regexes: Vec<Regex>,
    }

    /// Why the filter did or did not want a torrent.
    pub enum Filter {
        Wanted,
        NotWanted,
        Rejected,
    }

    /// Why a torrent is being considered, which decides whether the download
    /// filters get a say.
    ///
    /// Keeping this explicit matters: the two callers previously differed only in
    /// which method they happened to call, so nothing stopped the filter being
    /// reintroduced on the command path by accident. Now there is exactly one
    /// place that consults it, and the policy is unit-testable on its own.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Trigger {
        /// Seen on an announce channel, or sent by the owner as an announce line.
        /// Nobody vetted it, so the filter lists decide.
        Announcement,
        /// Named explicitly with `cmd:addtorrent` by an authorized user. The
        /// filters exist to choose what to take *unattended*; someone who names a
        /// torrent by hand has already made that choice, so they are skipped.
        ExplicitCommand,
    }

    impl Trigger {
        pub fn consults_filters(self) -> bool {
            matches!(self, Trigger::Announcement)
        }
    }

    /// What became of an announced torrent.
    ///
    /// This used to be a bare `bool`, so a torrent that simply matched none of
    /// the download patterns was reported as "Could not add torrent to client"
    /// -- blaming the client or the network when nothing had gone wrong.
    pub enum TorrentOutcome {
        Added,
        /// Matched none of `regex_for_downloads_match`.
        NotWanted,
        /// Matched `regex_for_downloads_reject_match`.
        Rejected,
        DownloadFailed(String),
        AddFailed(String),
    }

    impl std::fmt::Display for TorrentOutcome {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                TorrentOutcome::Added => write!(f, "Torrent added to client."),
                TorrentOutcome::NotWanted => write!(
                    f,
                    "Ignored: the name matches none of your regex_for_downloads_match patterns."
                ),
                TorrentOutcome::Rejected => {
                    write!(f, "Ignored: the name matches regex_for_downloads_reject_match.")
                }
                TorrentOutcome::DownloadFailed(e) => {
                    write!(f, "Could not download the torrent file: {e}")
                }
                TorrentOutcome::AddFailed(e) => write!(f, "Could not add torrent to client: {e}"),
            }
        }
    }

    impl TorrentProcessor {
        pub fn new(
            config: Rc<RefCell<Config>>,
            evt_channel: PubSub<String>,
            subs_cfg: Vec<Subscription<String>>,
            torrent_client: TorrentClientsEnum,
            torrent_platform: HttpTracker,
            notifier: Notifier,
        ) -> TorrentProcessor {
            // let dl_regex = config.lock().unwrap().get_dl_regexes().clone();
            Self {
                evt_channel,
                subs_cfg,
                torrent_client,
                torrent_platform,
                options: config,
                notifier,
                // dl_regexes: dl_regex,
            }
        }

        /// Handle a torrent seen on an announce channel: filters apply.
        pub async fn process_torrent(&self, name: &String, id: &String) -> TorrentOutcome {
            self.fetch_and_add(name, id, Trigger::Announcement).await
        }

        /// Fetch a torrent and hand it to the client.
        ///
        /// The single place the download filters are consulted, and only for the
        /// trigger that asks for them.
        async fn fetch_and_add(
            &self,
            name: &str,
            id: &str,
            trigger: Trigger,
        ) -> TorrentOutcome {
            if trigger.consults_filters() {
                match self.do_we_want_this_torrent(name) {
                    Filter::Wanted => {}
                    Filter::NotWanted => return TorrentOutcome::NotWanted,
                    Filter::Rejected => return TorrentOutcome::Rejected,
                }
            } else {
                info!("Adding '{name}' on request; download filters do not apply.");
            }

            let b64 = match self.download_torrent(name.to_string(), id.to_string()).await {
                Ok(b64) => {
                    info!("Torrent downloaded.");
                    b64
                }
                Err(e) => {
                    // Previously an `if let Ok(..)` with no else, so a failed
                    // download fell through to the same bare `false` as
                    // everything else and the reason was never surfaced.
                    error!("Could not download torrent '{name}': {e:?}");
                    self.notifier.send(Event::AddFailed {
                        name: name.to_string(),
                        reason: e.to_string(),
                    });
                    return TorrentOutcome::DownloadFailed(e.to_string());
                }
            };

            match self.add_torrent_and_start(b64, name.to_string()).await {
                Ok(_) => {
                    info!("Torrent added to client.");
                    self.notifier.send(Event::TorrentAdded(name.to_string()));
                    TorrentOutcome::Added
                }
                Err(e) => {
                    error!("Could not add torrent to client. {:?}", e);
                    self.notifier.send(Event::AddFailed {
                        name: name.to_string(),
                        reason: e.to_string(),
                    });
                    TorrentOutcome::AddFailed(e.to_string())
                }
            }
        }

        pub fn do_we_want_this_torrent(&self, name: &str) -> Filter {
            let dl_regexes = self.options.borrow().get_dl_regexes();
            let reject_regexes = self.options.borrow().get_reject_regexes();

            for regex in &reject_regexes {
                if regex.is_match(name) {
                    info!("Torrent '{name}' rejected by '{}'", regex.as_str());
                    return Filter::Rejected;
                }
            }
            for regex in &dl_regexes {
                if regex.is_match(name) {
                    info!("Torrent '{name}' matched '{}'", regex.as_str());
                    return Filter::Wanted;
                }
            }

            // Silent before, which left a filtered-out torrent looking exactly
            // like a broken client.
            info!(
                "Torrent '{name}' matches none of the {} download pattern(s); ignoring",
                dl_regexes.len()
            );
            Filter::NotWanted
        }

        /// `&self`, not `&mut self`: it only reads `torrent_client`, and the
        /// needless `&mut` put it out of reach of CommandProcessor, which holds
        /// an `Rc<TorrentProcessor>`. The list it returns is already owned, so
        /// the `to_owned()` on it was a second full copy.
        pub async fn get_download_list(&self) -> Result<Vec<TorrentInfo>, Error> {
            match &self.torrent_client {
                TorrentClientsEnum::Rtorrent(c) => c.get_torrent_info().await,
                TorrentClientsEnum::QBittorrent(c) => c.get_torrent_info().await,
                // Flood's list has the same information behind a different API.
                // Mapping its older name/size/date shape would report 0% for
                // everything, which is worse than saying so.
                TorrentClientsEnum::Flood(_) => Err(Error::msg(
                    "Listing torrents is implemented for rTorrent and qBittorrent only.",
                )),
            }
        }

        pub async fn add_torrent_and_start(&self, file: String, name: String) -> Result<(), Error> {
            match &self.torrent_client {
                TorrentClientsEnum::Rtorrent(c) => c.add_torrent_and_start(&file, name).await,
                TorrentClientsEnum::Flood(c) => c.add_torrent_and_start(&file, name).await,
                TorrentClientsEnum::QBittorrent(c) => c.add_torrent_and_start(&file, name).await,
            }
        }

        pub async fn download_torrent(&self, name: String, id: String) -> Result<String, Error> {
            // This used to match on a one-variant enum with an `else` arm that
            // reported "Torrent platform not supported" and was unreachable.
            self.torrent_platform.download_torrent(name, id).await
        }

        /// Add a torrent named by an explicit `addtorrent` command.
        ///
        /// Goes through the same path as an announcement but with
        /// `Trigger::ExplicitCommand`, so the download filters are not consulted
        /// -- an authorized user who names a torrent gets that torrent.
        pub async fn add_torrent(&self, name: &str, id: &str) -> Result<String, String> {
            // Both client arms of the old implementation were
            // `.expect("TODO: panic message")`, so any error killed the bot
            // instead of reaching whoever asked; its download error was
            // discarded the same way, reported as a fixed "Can not download
            // torrent file".
            match self.fetch_and_add(name, id, Trigger::ExplicitCommand).await {
                TorrentOutcome::Added => Ok(format!("Torrent {name} added.")),
                // Unreachable for this trigger, but mapping them keeps the match
                // exhaustive if another filtered trigger is ever added.
                other => Err(other.to_string()),
            }
        }

        /// Keep only the finished ones, and turn a failure into `None`.
        ///
        /// Shared by every client that can answer, so the distinction below is
        /// stated once rather than re-derived per arm.
        fn completed_rows(
            client: &str,
            rows: Result<Vec<crate::clients::CompletionRow>, Error>,
        ) -> Option<Vec<(String, String)>> {
            match rows {
                Ok(rows) => Some(
                    rows.into_iter()
                        .filter(|r| r.complete)
                        .map(|r| (r.hash, r.name))
                        .collect(),
                ),
                Err(e) => {
                    error!("Could not poll {client} for finished downloads: {e}");
                    None
                }
            }
        }

        /// Which torrents the client reports as finished, keyed by hash.
        ///
        /// `None` when the client cannot answer, which is different from "none
        /// are finished" -- the caller must not treat a failed poll as every
        /// torrent having disappeared. `notify::poll` retains its seen-set
        /// against this list, so an empty Vec on a transient failure would wipe
        /// the set and re-announce the whole finished library on the next tick.
        pub async fn get_completed(&self) -> Option<Vec<(String, String)>> {
            match &self.torrent_client {
                TorrentClientsEnum::Rtorrent(c) => {
                    Self::completed_rows("rTorrent", c.get_completion().await)
                }
                TorrentClientsEnum::QBittorrent(c) => {
                    Self::completed_rows("qBittorrent", c.get_completion().await)
                }
                // Flood exposes completion over its own HTTP API rather than
                // d.multicall2. Not wired up: guessing here would produce
                // phantom "finished" notifications rather than an honest gap.
                TorrentClientsEnum::Flood(_) => None,
            }
        }

        /// The watchlist, in the order `remove_torrent_from_watchlist` indexes it.
        pub fn get_watchlist(&self) -> Vec<String> {
            self.options.borrow().get_dl_patterns()
        }

        pub async fn add_torrent_to_watchlist(&self, argument: String) -> Result<String, String> {
            // The error is now reported instead of dropped: an invalid regex was
            // logged and discarded while this still answered "added".
            self.options
                .borrow()
                .add_dl_regex(argument.clone())
                .await
                .map_err(|e| e.to_string())?;
            Ok(format!("Added to watch list: {argument}"))
        }

        pub async fn remove_torrent_from_watchlist(&self, index: usize) -> Result<String, String> {
            // Read the pattern before removing it, so the reply names what went
            // rather than echoing back the index the caller already supplied.
            // Nothing awaits between this read and the removal, so the index
            // cannot go stale in between.
            let Some(removed) = self.options.borrow().get_dl_patterns().get(index).cloned() else {
                return Err(format!(
                    "No pattern at index {index}. Use cmd:watchlist to see the current list."
                ));
            };

            self.options.borrow().remove_dl_regex(index).await;
            Ok(format!("Removed [{index}] {removed}"))
        }
    }

    #[cfg(test)]
    mod test {
        use super::*;

        /// `cmd:addtorrent` names a specific torrent, so the download lists --
        /// which exist to pick releases out of an announce feed nobody is
        /// watching -- must not get a veto over it.
        #[test]
        fn an_explicit_command_does_not_consult_the_download_filters() {
            assert!(!Trigger::ExplicitCommand.consults_filters());
        }

        /// The converse still has to hold, or the announce channel would add
        /// everything the tracker publishes.
        #[test]
        fn an_announcement_still_consults_the_download_filters() {
            assert!(Trigger::Announcement.consults_filters());
        }
    }
}
