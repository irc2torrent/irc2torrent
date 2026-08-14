//! What a matched announce line carries.
//!
//! `regex_for_announce_match` is the one piece of configuration every user has
//! to write themselves, and it is already a description of their tracker's
//! announce format. This module makes it the description of the *data* too:
//! every named capture group becomes a field, so a network that announces an
//! uploader, a category or a freeleech marker exposes them without a line of
//! tracker-specific code -- which is the constraint the rest of the crate is
//! built around.
//!
//! Two names are structural. `name` and `id` are what the download URL and the
//! client are built from, so they are typed fields rather than map entries;
//! putting them in the map would mean every consumer unwrapping an `Option` for
//! a value that must always exist. `LoadedOptions::from_data` refuses a regex
//! that does not declare both, so the `Option` those accessors would return is
//! one the config layer has already eliminated.
//!
//! Two more are reserved: `file` and `key` belong to the download URL template
//! and are never taken from an announce line. A capture with either name is
//! dropped here and warned about at config load.

use std::collections::BTreeMap;

use regex::{Captures, Regex};
use serde_derive::{Deserialize, Serialize};

/// Captures whose names the URL template supplies itself.
pub const RESERVED_NAMES: [&str; 2] = ["file", "key"];

/// Captures a config must declare, because the crate cannot work without them.
pub const REQUIRED_NAMES: [&str; 2] = ["name", "id"];

/// Per-capture handling, from `[captures.<name>]` in options.toml.
///
/// A struct rather than a bare name-to-delimiter map so this can grow -- trim,
/// case folding -- without changing the shape of anyone's config file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureOptions {
    /// Split the captured text into several values on this delimiter.
    ///
    /// Only worth setting when the *count* is unknown, such as a comma-separated
    /// tag list. Two fixed sub-values are better expressed as two capture
    /// groups: `<(?P<category>[^:]+) :: (?P<subcategory>[^>]+)>` needs nothing
    /// here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split: Option<String>,
}

/// One captured field: the text as it appeared, and how it splits.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    /// Verbatim, so the download URL and log lines see what the tracker sent.
    raw: String,
    /// `split` applied. Exactly `[raw]` when no delimiter is configured.
    values: Vec<String>,
}

/// A release seen on an announce channel, or named by an explicit command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announce {
    pub name: String,
    pub id: String,
    /// Only the optional captures that **participated** in the match.
    ///
    /// `BTreeMap` for a deterministic order in logs and notifications, and
    /// because it derives `Hash`/`Eq` where `HashMap` does not.
    fields: BTreeMap<String, Field>,
}

impl Announce {
    /// Build from a successful match.
    ///
    /// `None` when `name` or `id` did not participate. Config validation makes
    /// that unreachable for a regex that declares them outright, but a group
    /// inside an alternation that did not fire still gets here -- and returning
    /// `None` is the whole point of this type, since the code this replaces
    /// indexed `caps["name"]` and panicked in the IRC read loop instead.
    pub fn from_captures(
        re: &Regex,
        caps: &Captures<'_>,
        options: &BTreeMap<String, CaptureOptions>,
    ) -> Option<Self> {
        let name = caps.name("name")?.as_str().to_owned();
        let id = caps.name("id")?.as_str().to_owned();

        let mut fields = BTreeMap::new();
        for capture_name in re.capture_names().flatten() {
            if REQUIRED_NAMES.contains(&capture_name) || RESERVED_NAMES.contains(&capture_name) {
                continue;
            }
            // Absent, not empty: a group that did not participate leaves no key
            // at all. That is what makes `(?P<freeleech>freeleech)?` a usable
            // boolean -- present means the marker was on the line.
            let Some(m) = caps.name(capture_name) else {
                continue;
            };

            let raw = m.as_str().to_owned();
            let values = match options.get(capture_name).and_then(|o| o.split.as_deref()) {
                Some(delim) if !delim.is_empty() => raw
                    .split(delim)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect(),
                _ => vec![raw.clone()],
            };

            fields.insert(capture_name.to_owned(), Field { raw, values });
        }

        Some(Self { name, id, fields })
    }

    /// Build from a name and id with no captured fields.
    ///
    /// The download path's tests use this, and `bare` is a thin wrapper over
    /// it. Production code otherwise goes through `from_captures`.
    pub fn new(name: String, id: String) -> Self {
        Self { name, id, fields: BTreeMap::new() }
    }

    /// A torrent named by id alone, with no announce line behind it.
    ///
    /// `cmd:addtorrent` accepts a bare link or id, which carries no release
    /// name and no fields -- ever. Named so that fact is greppable rather than
    /// implied by an empty map at a call site.
    pub fn bare(id: String) -> Self {
        Self::new(format!("torrent-{id}"), id)
    }

    /// The field's text as the tracker sent it.
    ///
    /// `name` and `id` answer here too, so a caller substituting placeholders
    /// has one lookup rather than three cases.
    pub fn get(&self, key: &str) -> Option<&str> {
        match key {
            "name" => Some(&self.name),
            "id" => Some(&self.id),
            k => self.fields.get(k).map(|f| f.raw.as_str()),
        }
    }

    /// The field's values, split if `[captures.<key>].split` is set.
    ///
    /// Empty for a field that is not present, so a caller iterating does not
    /// need to check first.
    pub fn values(&self, key: &str) -> &[String] {
        match key {
            "name" => std::slice::from_ref(&self.name),
            "id" => std::slice::from_ref(&self.id),
            k => self.fields.get(k).map_or(&[], |f| f.values.as_slice()),
        }
    }

    /// Whether the capture participated.
    ///
    /// The test for a marker-style group: `has("freeleech")`.
    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Present field names, excluding `name` and `id`. Sorted.
    pub fn field_names(&self) -> impl Iterator<Item = &str> {
        self.fields.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// The TorrentLeech format, with every optional group the sample uses.
    const TL: &str = r"<(?P<category>[^:]+) :: (?P<subcategory>[^>]+)>\s+Name:'(?P<name>.*)' uploaded by '(?P<uploader>[^']+)'(?P<freeleech> freeleech)?.*/torrent/(?P<id>\d+)";

    const FREELEECH_LINE: &str = "New Torrent Announcement: <Movies :: 4K>  Name:'Barbarian 2022 2160p x265-j3rico' uploaded by 'j3rico' freeleech -  https://www.torrentleech.org/torrent/241816501";
    const PLAIN_LINE: &str = "New Torrent Announcement: <TV :: Episodes HD>  Name:'Some Show S01E01 1080p' uploaded by 'someone' -  https://www.torrentleech.org/torrent/999";

    fn parse(line: &str, options: &BTreeMap<String, CaptureOptions>) -> Announce {
        let re = Regex::new(TL).unwrap();
        let caps = re.captures(line).expect("line should match");
        Announce::from_captures(&re, &caps, options).expect("name and id participate")
    }

    fn no_options() -> BTreeMap<String, CaptureOptions> {
        BTreeMap::new()
    }

    #[test]
    fn required_captures_become_typed_fields() {
        let a = parse(FREELEECH_LINE, &no_options());
        assert_eq!(a.name, "Barbarian 2022 2160p x265-j3rico");
        assert_eq!(a.id, "241816501");
    }

    #[test]
    fn optional_captures_are_collected() {
        let a = parse(FREELEECH_LINE, &no_options());
        assert_eq!(a.get("uploader"), Some("j3rico"));
        assert_eq!(a.get("category"), Some("Movies"));
        assert_eq!(a.get("subcategory"), Some("4K"));
    }

    #[test]
    fn a_group_that_did_not_participate_leaves_no_key() {
        // The whole point of the marker model: absent means the word was not on
        // the line, and `has` is the test for it.
        let free = parse(FREELEECH_LINE, &no_options());
        let plain = parse(PLAIN_LINE, &no_options());

        assert!(free.has("freeleech"), "freeleech line should carry the marker");
        assert!(!plain.has("freeleech"), "plain line should not");
        assert_eq!(plain.get("freeleech"), None);
    }

    #[test]
    fn a_group_that_matched_empty_is_present_but_empty() {
        // Distinct from not participating, and deliberately so: this one did
        // fire, it just captured nothing.
        let re = Regex::new(r"(?P<name>\w+) (?P<id>\d+)(?P<note>.*)").unwrap();
        let caps = re.captures("release 42").unwrap();
        let a = Announce::from_captures(&re, &caps, &no_options()).unwrap();

        assert!(a.has("note"));
        assert_eq!(a.get("note"), Some(""));
    }

    #[test]
    fn name_and_id_are_not_duplicated_into_the_map() {
        let a = parse(FREELEECH_LINE, &no_options());
        let names: Vec<&str> = a.field_names().collect();
        assert!(!names.contains(&"name"), "{names:?}");
        assert!(!names.contains(&"id"), "{names:?}");
    }

    #[test]
    fn reserved_names_are_dropped_rather_than_shadowing_the_url_template() {
        let re = Regex::new(r"(?P<name>\w+) (?P<id>\d+) (?P<key>\w+) (?P<file>\w+)").unwrap();
        let caps = re.captures("release 42 leaked stolen").unwrap();
        let a = Announce::from_captures(&re, &caps, &no_options()).unwrap();

        assert_eq!(a.get("key"), None, "an announce line must never supply rss_key");
        assert_eq!(a.get("file"), None);
        assert_eq!(a.field_names().count(), 0);
    }

    #[test]
    fn a_missing_required_capture_yields_none_rather_than_panicking() {
        // The regression this type exists for: the code it replaces indexed
        // caps["name"] and panicked inside the IRC read loop.
        let re = Regex::new(r"(?:(?P<name>a)|(?P<other>b)) (?P<id>\d+)").unwrap();
        let caps = re.captures("b 42").unwrap();
        assert!(Announce::from_captures(&re, &caps, &no_options()).is_none());
    }

    #[test]
    fn split_produces_several_values_and_leaves_raw_alone() {
        let mut options = BTreeMap::new();
        options.insert(
            "tags".to_string(),
            CaptureOptions { split: Some(",".to_string()) },
        );

        let re = Regex::new(r"(?P<name>\w+) (?P<id>\d+) (?P<tags>.*)").unwrap();
        let caps = re.captures("release 42 hd, remux , dv").unwrap();
        let a = Announce::from_captures(&re, &caps, &options).unwrap();

        assert_eq!(a.values("tags"), ["hd", "remux", "dv"], "split and trimmed");
        assert_eq!(a.get("tags"), Some("hd, remux , dv"), "raw is untouched");
    }

    #[test]
    fn an_unsplit_field_is_a_single_value() {
        let a = parse(FREELEECH_LINE, &no_options());
        assert_eq!(a.values("category"), ["Movies"]);
    }

    #[test]
    fn splitting_drops_empty_segments() {
        // A trailing or doubled delimiter is a formatting artefact, not a tag.
        let mut options = BTreeMap::new();
        options.insert("tags".to_string(), CaptureOptions { split: Some(",".to_string()) });

        let re = Regex::new(r"(?P<name>\w+) (?P<id>\d+) (?P<tags>.*)").unwrap();
        let caps = re.captures("release 42 hd,,remux,").unwrap();
        let a = Announce::from_captures(&re, &caps, &options).unwrap();

        assert_eq!(a.values("tags"), ["hd", "remux"]);
    }

    #[test]
    fn values_of_an_absent_field_is_empty_not_a_panic() {
        let a = parse(PLAIN_LINE, &no_options());
        assert!(a.values("freeleech").is_empty());
        assert!(a.values("nonexistent").is_empty());
    }

    #[test]
    fn name_and_id_answer_both_accessors() {
        let a = parse(PLAIN_LINE, &no_options());
        assert_eq!(a.get("name"), Some("Some Show S01E01 1080p"));
        assert_eq!(a.values("id"), ["999"]);
        assert!(a.has("name") && a.has("id"));
    }

    #[test]
    fn a_bare_id_carries_a_synthetic_name_and_no_fields() {
        let a = Announce::bare("241816501".to_string());
        assert_eq!(a.name, "torrent-241816501");
        assert_eq!(a.id, "241816501");
        assert_eq!(a.field_names().count(), 0);
        assert!(!a.has("freeleech"));
    }
}
