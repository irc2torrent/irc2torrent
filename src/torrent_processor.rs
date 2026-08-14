pub mod torrent {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    use anyhow::Error;
    use base64;
    use base64::Engine as _;
    use log::{error, info, warn};
    use pub_sub::{PubSub, Subscription};
    use regex::Regex;

    use crate::announce::Announce;
    use crate::clients::{DownloadResult, TorrentClientsEnum, TorrentInfo};
    use crate::config::config::{CompiledFieldFilter, CompiledWatch, Config};
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
        /// Wanted by name, vetoed by a field rule. Carries its own description
        /// because the caller reports it verbatim.
        FieldRejected(String),
    }

    /// What the filters decided, and which pattern decided it.
    ///
    /// Separate from `Filter` because the caller only ever acts on
    /// wanted/rejected/not-wanted, while the *reason* exists purely to be
    /// reported. Keeping it separate also makes the decision a pure function of
    /// the two pattern lists, which is the only way to test it: building a real
    /// `TorrentProcessor` needs a live torrent client.
    #[derive(Debug, PartialEq)]
    enum Decision<'a> {
        Wanted(&'a str),
        /// `overrode` is the download pattern that also matched, if any -- the
        /// difference between "the filter worked" and "your reject list is
        /// eating things you asked for".
        Rejected {
            by: &'a str,
            overrode: Option<&'a str>,
        },
        NotWanted,
        /// A captured field vetoed a release the name lists had already wanted.
        FieldRejected {
            field: &'a str,
            why: FieldVeto<'a>,
        },
    }

    /// Why a field rule vetoed a release.
    #[derive(Debug, PartialEq)]
    enum FieldVeto<'a> {
        /// Named in `require_fields`, but the capture did not participate.
        Absent,
        /// `matches` is set and no value matched it. An absent field lands here
        /// too: there is nothing to test, so it cannot pass.
        NoValueMatched(&'a str),
        /// `reject_matching` hit one of the values.
        RejectedBy(&'a str),
    }

    impl std::fmt::Display for FieldVeto<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                FieldVeto::Absent => write!(f, "it is not on the announce line"),
                FieldVeto::NoValueMatched(p) => write!(f, "no value matches '{p}'"),
                FieldVeto::RejectedBy(p) => write!(f, "a value matches '{p}'"),
            }
        }
    }

    /// Reject wins over match, as it always has. The download list is consulted
    /// afterwards only to explain *why*, and only once a reject has already
    /// hit -- so the ordinary path costs exactly what it did before.
    ///
    /// Field rules run last, as a veto over a release the name lists already
    /// wanted. Ordering them that way keeps `Rejected { overrode }` meaning
    /// exactly what it did -- "your reject list ate something you asked for" --
    /// rather than becoming a second, differently-shaped rejection source.
    fn decide<'a>(
        announce: &Announce,
        dl: &'a [CompiledWatch],
        reject: &'a [Regex],
        require_fields: &'a [String],
        field_filters: &'a [(String, CompiledFieldFilter)],
    ) -> Decision<'a> {
        let name = announce.name.as_str();

        // The global reject list wins over everything, as it always has.
        if let Some(r) = reject.iter().find(|r| r.is_match(name)) {
            return Decision::Rejected {
                by: r.as_str(),
                overrode: dl
                    .iter()
                    .find(|w| w.regex.is_match(name))
                    .map(|w| w.regex.as_str()),
            };
        }

        let matched: Vec<&CompiledWatch> =
            dl.iter().filter(|w| w.regex.is_match(name)).collect();
        let Some(first) = matched.first() else {
            return Decision::NotWanted;
        };

        // A field named by ANY matching entry is that entry's to decide: the
        // global rule for it is dropped rather than added to, so a line saying
        // "this one, freeleech or not" is not silently overruled by a blanket
        // require_fields. Global rules survive only for fields no matching
        // entry mentions.
        let overridden: Vec<&str> = matched
            .iter()
            .flat_map(|w| {
                w.require_fields
                    .iter()
                    .map(String::as_str)
                    .chain(w.field_filters.iter().map(|(f, _)| f.as_str()))
            })
            .collect();

        // Where several matching entries name the same field, all of their
        // rules apply -- overlapping lines compound rather than one winning.
        let required = require_fields
            .iter()
            .filter(|f| !overridden.contains(&f.as_str()))
            .chain(matched.iter().flat_map(|w| w.require_fields.iter()));

        for field in required {
            if !announce.has(field) {
                return Decision::FieldRejected { field, why: FieldVeto::Absent };
            }
        }

        let filters = field_filters
            .iter()
            .filter(|(f, _)| !overridden.contains(&f.as_str()))
            .chain(matched.iter().flat_map(|w| w.field_filters.iter()));

        for (field, filter) in filters {
            // `values` is per-element for a split capture and a single element
            // otherwise, so one tag out of a list is enough either way. It is
            // empty for an absent field, which is what gives the two rules
            // their opposite answers below.
            let values = announce.values(field);

            if let Some(re) = &filter.matches {
                if !values.iter().any(|v| re.is_match(v)) {
                    return Decision::FieldRejected {
                        field,
                        why: FieldVeto::NoValueMatched(re.as_str()),
                    };
                }
            }
            if let Some(re) = &filter.reject_matching {
                if values.iter().any(|v| re.is_match(v)) {
                    return Decision::FieldRejected {
                        field,
                        why: FieldVeto::RejectedBy(re.as_str()),
                    };
                }
            }
        }

        // The first matching pattern is what gets named, as before.
        Decision::Wanted(first.regex.as_str())
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
        /// Matched by name, then vetoed by `require_fields` or a
        /// `[field_filters.*]` rule. Deliberately not folded into `Rejected`,
        /// whose message names `regex_for_downloads_reject_match` and would be
        /// pointing at the wrong setting.
        FieldRejected(String),
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
                TorrentOutcome::FieldRejected(why) => write!(f, "Ignored: {why}"),
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
        pub async fn process_torrent(&self, announce: &Announce) -> TorrentOutcome {
            self.fetch_and_add(announce, Trigger::Announcement).await
        }

        /// Fetch a torrent and hand it to the client.
        ///
        /// The single place the download filters are consulted, and only for the
        /// trigger that asks for them.
        async fn fetch_and_add(
            &self,
            announce: &Announce,
            trigger: Trigger,
        ) -> TorrentOutcome {
            let name = announce.name.as_str();
            if trigger.consults_filters() {
                match self.do_we_want_this_torrent(announce) {
                    Filter::Wanted => {}
                    Filter::NotWanted => return TorrentOutcome::NotWanted,
                    Filter::Rejected => return TorrentOutcome::Rejected,
                    Filter::FieldRejected(why) => return TorrentOutcome::FieldRejected(why),
                }
            } else {
                info!("Adding '{name}' on request; download filters do not apply.");
            }

            let b64 = match self.download_torrent(announce).await {
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

        pub fn do_we_want_this_torrent(&self, announce: &Announce) -> Filter {
            let name = announce.name.as_str();
            let dl_watches = self.options.borrow().get_dl_watches();
            let reject_regexes = self.options.borrow().get_reject_regexes();
            let require_fields = self.options.borrow().get_require_fields();
            let field_filters = self.options.borrow().get_field_filters();

            match decide(
                announce,
                &dl_watches,
                &reject_regexes,
                &require_fields,
                &field_filters,
            ) {
                // The case worth interrupting someone for: a pattern the user
                // wrote to catch this release did catch it, and a reject pattern
                // threw it away anyway. Either they meant that, or a reject
                // pattern is broader than they realised and is quietly eating
                // releases they asked for -- and nothing else in the log tells
                // those two apart.
                Decision::Rejected { by, overrode: Some(wanted) } => {
                    warn!(
                        "Torrent '{name}' matched '{wanted}' but was rejected by '{by}'; not downloading"
                    );
                    // Gated by `on_warning`, off by default. Deduplicated on the
                    // pair of patterns rather than the release name, so an
                    // over-broad reject rule reads as one fact with a count
                    // instead of one message per release it ate.
                    self.notifier.send(Event::RejectedDespiteMatch {
                        name: name.to_string(),
                        wanted: wanted.to_string(),
                        reject: by.to_string(),
                    });
                    Filter::Rejected
                }
                // A reject discarding something no pattern asked for is just the
                // filter working. Noise at anything above info.
                Decision::Rejected { by, overrode: None } => {
                    info!("Torrent '{name}' rejected by '{by}'");
                    Filter::Rejected
                }
                Decision::Wanted(pattern) => {
                    info!("Torrent '{name}' matched '{pattern}'");
                    Filter::Wanted
                }
                // Its own arm rather than folding into Rejected: the name
                // matched, so pointing at regex_for_downloads_reject_match
                // would send someone to the wrong setting.
                Decision::FieldRejected { field, why } => {
                    let reason = format!("`{field}` vetoed it: {why}");
                    info!("Torrent '{name}' wanted by name, but {reason}");
                    Filter::FieldRejected(reason)
                }
                // Silent before, which left a filtered-out torrent looking
                // exactly like a broken client.
                Decision::NotWanted => {
                    info!(
                        "Torrent '{name}' matches none of the {} download pattern(s); ignoring",
                        dl_watches.len()
                    );
                    Filter::NotWanted
                }
            }
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

        pub async fn download_torrent(&self, announce: &Announce) -> Result<String, Error> {
            // This used to match on a one-variant enum with an `else` arm that
            // reported "Torrent platform not supported" and was unreachable.
            self.torrent_platform.download_torrent(announce).await
        }

        /// Add a torrent named by an explicit `addtorrent` command.
        ///
        /// Goes through the same path as an announcement but with
        /// `Trigger::ExplicitCommand`, so the download filters are not consulted
        /// -- an authorized user who names a torrent gets that torrent.
        pub async fn add_torrent(&self, announce: &Announce) -> Result<String, String> {
            let name = announce.name.as_str();
            // Both client arms of the old implementation were
            // `.expect("TODO: panic message")`, so any error killed the bot
            // instead of reaching whoever asked; its download error was
            // discarded the same way, reported as a fixed "Can not download
            // torrent file".
            match self.fetch_and_add(announce, Trigger::ExplicitCommand).await {
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

        fn regexes(patterns: &[&str]) -> Vec<Regex> {
            patterns.iter().map(|p| Regex::new(p).unwrap()).collect()
        }

        /// Watch entries carrying no rules of their own.
        fn watches(patterns: &[&str]) -> Vec<CompiledWatch> {
            patterns
                .iter()
                .map(|p| CompiledWatch {
                    regex: Regex::new(p).unwrap(),
                    require_fields: Vec::new(),
                    field_filters: Vec::new(),
                })
                .collect()
        }

        /// `decide` with no field rules and a synthetic announce, so the
        /// pre-existing name-list cases below read exactly as they did.
        fn decide_by_name<'a>(
            name: &str,
            dl: &'a [CompiledWatch],
            reject: &'a [Regex],
        ) -> Decision<'a> {
            decide(&Announce::new(name.to_string(), "1".to_string()), dl, reject, &[], &[])
        }

        /// The distinction the warning exists for: a release the user's own
        /// match list asked for, thrown away by a reject pattern.
        #[test]
        fn a_reject_that_overrode_a_match_says_which_pattern_wanted_it() {
            let dl = watches(&[".*2160p.*"]);
            let reject = regexes(&["(?i).*GERMAN.*"]);
            assert_eq!(
                decide_by_name("Some.Release.2160p.GERMAN.WEB", &dl, &reject),
                Decision::Rejected { by: "(?i).*GERMAN.*", overrode: Some(".*2160p.*") }
            );
        }

        /// The ordinary case, which must stay quiet: nothing asked for it, so
        /// the reject pattern discarding it is the filter doing its job.
        #[test]
        fn a_reject_of_something_unwanted_overrode_nothing() {
            let dl = watches(&[".*2160p.*"]);
            let reject = regexes(&["(?i).*GERMAN.*"]);
            assert_eq!(
                decide_by_name("Some.Release.1080p.GERMAN.WEB", &dl, &reject),
                Decision::Rejected { by: "(?i).*GERMAN.*", overrode: None }
            );
        }

        /// Reject still wins over match -- the warning reports the outcome, it
        /// does not change it.
        #[test]
        fn reject_still_beats_match() {
            let dl = watches(&[".*2160p.*"]);
            let reject = regexes(&["(?i).*GERMAN.*"]);
            assert!(matches!(
                decide_by_name("Some.Release.2160p.GERMAN.WEB", &dl, &reject),
                Decision::Rejected { .. }
            ));
        }

        #[test]
        fn a_plain_match_names_the_pattern_that_matched() {
            let dl = watches(&[".*1080p.*", ".*2160p.*"]);
            assert_eq!(
                decide_by_name("Some.Release.2160p.WEB", &dl, &[]),
                Decision::Wanted(".*2160p.*")
            );
        }

        #[test]
        fn matching_nothing_is_not_wanted() {
            let dl = watches(&[".*2160p.*"]);
            let reject = regexes(&["(?i).*GERMAN.*"]);
            assert_eq!(decide_by_name("Some.Release.720p.WEB", &dl, &reject), Decision::NotWanted);
        }

        // ---- field rules -------------------------------------------------

        /// An announce carrying the given fields, matched by `.*2160p.*` below.
        fn announce_with(fields: &[(&str, &str)]) -> Announce {
            announce_named("Some.Release.2160p.WEB", fields)
        }

        /// As above, with a chosen release name -- for the cases about which
        /// entries a name matches.
        fn announce_named(name: &str, fields: &[(&str, &str)]) -> Announce {
            let names: Vec<String> = fields.iter().map(|(k, _)| (*k).to_string()).collect();
            let pattern = format!(
                r"(?P<name>[^|]+)\|{}\|(?P<id>\d+)",
                names
                    .iter()
                    .map(|n| format!("(?P<{n}>[^|]*)"))
                    .collect::<Vec<_>>()
                    .join(r"\|")
            );
            let re = Regex::new(&pattern).unwrap();
            let subject = format!(
                "{name}|{}|7",
                fields.iter().map(|(_, v)| *v).collect::<Vec<_>>().join("|")
            );
            let caps = re.captures(&subject).expect("fixture should match");
            Announce::from_captures(&re, &caps, &Default::default()).unwrap()
        }

        fn filter(matches: Option<&str>, reject_matching: Option<&str>) -> CompiledFieldFilter {
            CompiledFieldFilter {
                matches: matches.map(|p| Regex::new(p).unwrap()),
                reject_matching: reject_matching.map(|p| Regex::new(p).unwrap()),
            }
        }

        #[test]
        fn a_required_field_that_is_present_passes() {
            let dl = watches(&[".*2160p.*"]);
            let a = announce_with(&[("freeleech", "freeleech")]);
            let require = vec!["freeleech".to_string()];
            assert_eq!(
                decide(&a, &dl, &[], &require, &[]),
                Decision::Wanted(".*2160p.*")
            );
        }

        #[test]
        fn a_required_field_that_did_not_capture_vetoes_it() {
            // The motivating case: only take freeleech releases.
            let dl = watches(&[".*2160p.*"]);
            let a = Announce::new("Some.Release.2160p.WEB".to_string(), "7".to_string());
            let require = vec!["freeleech".to_string()];
            assert_eq!(
                decide(&a, &dl, &[], &require, &[]),
                Decision::FieldRejected { field: "freeleech", why: FieldVeto::Absent }
            );
        }

        #[test]
        fn field_rules_run_only_after_the_name_lists_agree() {
            // A release nothing asked for is NotWanted, not FieldRejected --
            // otherwise every unwanted release would be reported as a field
            // problem.
            let dl = watches(&[".*2160p.*"]);
            let a = Announce::new("Some.Release.720p.WEB".to_string(), "7".to_string());
            let require = vec!["freeleech".to_string()];
            assert_eq!(decide(&a, &dl, &[], &require, &[]), Decision::NotWanted);
        }

        #[test]
        fn a_reject_pattern_still_wins_over_a_field_rule() {
            let dl = watches(&[".*2160p.*"]);
            let reject = regexes(&["(?i).*GERMAN.*"]);
            let a = announce_named("Some.Release.2160p.GERMAN.WEB", &[("freeleech", "freeleech")]);
            assert!(matches!(
                decide(&a, &dl, &reject, &[], &[]),
                Decision::Rejected { .. }
            ));
        }

        #[test]
        fn matches_takes_only_what_it_names() {
            let dl = watches(&[".*2160p.*"]);
            let rules = vec![("category".to_string(), filter(Some("^Movies$"), None))];

            let movie = announce_with(&[("category", "Movies")]);
            assert_eq!(decide(&movie, &dl, &[], &[], &rules), Decision::Wanted(".*2160p.*"));

            let tv = announce_with(&[("category", "TV")]);
            assert_eq!(
                decide(&tv, &dl, &[], &[], &rules),
                Decision::FieldRejected {
                    field: "category",
                    why: FieldVeto::NoValueMatched("^Movies$")
                }
            );
        }

        #[test]
        fn reject_matching_drops_what_it_names() {
            let dl = watches(&[".*2160p.*"]);
            let rules = vec![("uploader".to_string(), filter(None, Some("^(?i)anonymous$")))];

            let named = announce_with(&[("uploader", "j3rico")]);
            assert_eq!(decide(&named, &dl, &[], &[], &rules), Decision::Wanted(".*2160p.*"));

            let anon = announce_with(&[("uploader", "Anonymous")]);
            assert!(matches!(
                decide(&anon, &dl, &[], &[], &rules),
                Decision::FieldRejected { field: "uploader", why: FieldVeto::RejectedBy(_) }
            ));
        }

        /// The two rules answer an absent field oppositely, and neither answer
        /// is obvious -- so both are pinned here rather than left to be
        /// rediscovered from the implementation.
        #[test]
        fn an_absent_field_fails_matches_and_passes_reject_matching() {
            let dl = watches(&[".*2160p.*"]);
            let a = Announce::new("Some.Release.2160p.WEB".to_string(), "7".to_string());

            // Nothing to test against, so it cannot pass.
            let rules = vec![("category".to_string(), filter(Some("^Movies$"), None))];
            assert!(matches!(
                decide(&a, &dl, &[], &[], &rules),
                Decision::FieldRejected { .. }
            ));

            // A rule about what a value looks like cannot fire on no value.
            let rules = vec![("category".to_string(), filter(None, Some("^TV$")))];
            assert_eq!(decide(&a, &dl, &[], &[], &rules), Decision::Wanted(".*2160p.*"));
        }

        // ---- global vs per-entry composition -----------------------------

        /// A watch entry carrying its own rules.
        fn watch(pattern: &str, require: &[&str], filters: &[(&str, CompiledFieldFilter)]) -> CompiledWatch {
            CompiledWatch {
                regex: Regex::new(pattern).unwrap(),
                require_fields: require.iter().map(|s| (*s).to_string()).collect(),
                field_filters: filters.iter().map(|(f, r)| ((*f).to_string(), r.clone())).collect(),
            }
        }

        #[test]
        fn a_global_rule_applies_to_an_entry_that_names_nothing() {
            let dl = watches(&[".*2160p.*"]);
            let a = Announce::new("Some.Release.2160p.WEB".to_string(), "7".to_string());
            let require = vec!["freeleech".to_string()];
            assert_eq!(
                decide(&a, &dl, &[], &require, &[]),
                Decision::FieldRejected { field: "freeleech", why: FieldVeto::Absent }
            );
        }

        /// The gap this rework closed: a blanket require_fields used to apply
        /// to every pattern, so "only freeleech for the 4K stuff" was
        /// unexpressible.
        #[test]
        fn an_entry_rule_replaces_the_global_rule_for_the_field_it_names() {
            // Global demands freeleech; this entry says it does not care, by
            // naming the field with no requirement of its own.
            let dl = vec![watch(
                ".*1080p.*",
                &[],
                &[("freeleech", filter(None, Some("^never$")))],
            )];
            let a = Announce::new("Some.Release.1080p.WEB".to_string(), "7".to_string());
            let require = vec!["freeleech".to_string()];

            // Global would have vetoed it; the entry owns `freeleech` now.
            assert_eq!(decide(&a, &dl, &[], &require, &[]), Decision::Wanted(".*1080p.*"));
        }

        #[test]
        fn a_global_rule_survives_for_a_field_no_matching_entry_mentions() {
            let dl = vec![watch(".*2160p.*", &["freeleech"], &[])];
            let a = announce_with(&[("freeleech", "freeleech")]);
            // Global names `category`, which no entry mentions, so it stands.
            let rules = vec![("category".to_string(), filter(Some("^Movies$"), None))];
            assert!(matches!(
                decide(&a, &dl, &[], &[], &rules),
                Decision::FieldRejected { field: "category", .. }
            ));
        }

        #[test]
        fn several_matching_entries_all_have_to_be_satisfied() {
            // Overlapping patterns compound rather than the first winning.
            let dl = vec![
                watch(".*2160p.*", &["freeleech"], &[]),
                watch("Star Trek.*", &["internal"], &[]),
            ];
            let a = announce_named("Star Trek S04E01 2160p WEB", &[("freeleech", "freeleech")]);

            // Matches both; the second demands a field this release lacks.
            assert_eq!(
                decide(&a, &dl, &[], &[], &[]),
                Decision::FieldRejected { field: "internal", why: FieldVeto::Absent }
            );
        }

        #[test]
        fn the_reject_list_still_wins_over_every_entry_rule() {
            // Even an entry that would have accepted it.
            let dl = vec![watch(".*2160p.*", &[], &[])];
            let reject = regexes(&["(?i).*GERMAN.*"]);
            let a = Announce::new("Some.Release.2160p.GERMAN".to_string(), "7".to_string());
            assert!(matches!(
                decide(&a, &dl, &reject, &[], &[]),
                Decision::Rejected { .. }
            ));
        }

        #[test]
        fn the_named_pattern_is_the_first_that_matched() {
            let dl = watches(&[".*2160p.*", "Some.*"]);
            let a = Announce::new("Some.Release.2160p.WEB".to_string(), "7".to_string());
            assert_eq!(decide(&a, &dl, &[], &[], &[]), Decision::Wanted(".*2160p.*"));
        }

        #[test]
        fn a_split_field_is_tested_per_value() {
            use crate::announce::CaptureOptions;
            let mut options = std::collections::BTreeMap::new();
            options.insert("tags".to_string(), CaptureOptions { split: Some(",".to_string()) });

            let re = Regex::new(r"(?P<name>[^|]+)\|(?P<tags>[^|]*)\|(?P<id>\d+)").unwrap();
            let caps = re.captures("Some.Release.2160p.WEB|hd, remux, dv|7").unwrap();
            let a = Announce::from_captures(&re, &caps, &options).unwrap();

            let dl = watches(&[".*2160p.*"]);

            // One element matching is enough for either rule.
            let rules = vec![("tags".to_string(), filter(Some("^remux$"), None))];
            assert_eq!(decide(&a, &dl, &[], &[], &rules), Decision::Wanted(".*2160p.*"));

            let rules = vec![("tags".to_string(), filter(None, Some("^dv$")))];
            assert!(matches!(
                decide(&a, &dl, &[], &[], &rules),
                Decision::FieldRejected { .. }
            ));
        }
    }
}
