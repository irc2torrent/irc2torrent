//! The download URL, built from a user-supplied template.
//!
//! # Why this is not a `str::replace`
//!
//! `{name}` and `{id}` arrive **verbatim from an IRC message** — from anyone who
//! can post in the announce channel. Substituting them into a URL naively hands
//! that person the request target: a release name of `@evil.example/` moves the
//! host, and because `rss_key` is part of the same URL, the operator's tracker
//! credential is then sent to a server the attacker picked.
//!
//! That is the same class of bug `sanitize_torrent_filename` exists for, one
//! layer up, so this module uses the same shape: sanitise, then assert.
//!
//!   * [`UrlTemplate::parse`] runs once at config load. It rejects a template
//!     that is not an absolute http(s) URL, and — importantly — one that puts a
//!     placeholder anywhere in the authority, where no amount of escaping would
//!     make substitution safe.
//!   * [`UrlTemplate::render`] percent-encodes every substituted value down to
//!     RFC 3986 *unreserved* characters, so a value cannot leave the syntactic
//!     position the template put it in, and then re-checks the authority anyway.
//!
//! No error in this module ever contains the rendered URL, because the key is
//! in it.

use anyhow::Error;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::Url;

use crate::template::{lex, FieldSource, Part};

/// RFC 3986 "unreserved": `ALPHA / DIGIT / "-" / "." / "_" / "~"`.
///
/// Everything else is escaped, which is what makes a substituted value unable to
/// introduce a `/`, `?`, `#`, `@`, `:` or `&` and so unable to change the URL's
/// structure. Leaving the four punctuation marks unescaped keeps ordinary
/// release names and filenames readable in the request, and none of them are
/// delimiters in any component.
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Placeholders the download layer supplies itself, whatever the announce
/// regex declares. Everything else must be a named capture.
pub(crate) const RESERVED: [&str; 2] = ["file", "key"];

const PLACEHOLDER_HELP: &str = "`{file}` and `{key}` are always available; any other placeholder \
     must name a capture group in regex_for_announce_match";

/// Stand-in prefix used only while validating the template.
const PROBE_PREFIX: &str = "qqzzprobe";

/// A probe for placeholder `idx`.
///
/// Built from a fixed alphabet rather than from the placeholder's own name, so
/// it is lowercase alphanumeric **by construction** for any name a user picks.
/// That is what makes the authority scan meaningful: a probe survives both URL
/// parsing and `UNRESERVED` encoding unchanged, so finding one in the authority
/// proves the placeholder sits somewhere substitution can never be made safe.
///
/// The trailing `q` is a terminator: without it `{a}`'s probe would be a prefix
/// of `{ab}`'s and an authority error could name the wrong placeholder.
fn probe_for(prefix: &str, idx: usize) -> String {
    format!("{prefix}{idx}q")
}

/// A probe prefix that does not already occur in the template's literal text.
///
/// A template that happens to contain `qqzzprobe` would otherwise make the
/// authority scan blame a placeholder that is not there. Fails closed either
/// way -- only the message would be wrong -- but the literal is finite, so
/// lengthening until it cannot collide is cheap and terminates.
fn probe_prefix_for(literals: &str) -> String {
    let mut prefix = PROBE_PREFIX.to_string();
    while literals.contains(&prefix) {
        prefix.insert(1, 'q');
    }
    prefix
}

#[derive(Debug)]
pub(crate) struct UrlTemplate {
    parts: Vec<Part>,
    /// Distinct placeholder names, in first-use order. `Part::Field` indexes it.
    names: Vec<String>,
    /// Scheme + userinfo + host + port of the template, captured at parse time.
    /// `render` refuses to return a URL whose authority differs from this.
    authority: String,
    uses_key: bool,
}

/// Everything that decides *where* a request goes, in one comparable string.
fn authority_of(u: &Url) -> String {
    format!(
        "{}://{}:{}@{}:{}",
        u.scheme(),
        u.username(),
        u.password().unwrap_or(""),
        u.host_str().unwrap_or(""),
        u.port_or_known_default()
            .map(|p| p.to_string())
            .unwrap_or_default(),
    )
}

impl UrlTemplate {
    pub fn parse(template: &str) -> Result<Self, Error> {
        if template.trim().is_empty() {
            return Err(Error::msg(format!(
                "download_url_template is empty; set it to the URL your tracker \
                 serves .torrent files from. {PLACEHOLDER_HELP}"
            )));
        }

        let (parts, names) = lex(template, "download_url_template")?;
        let uses_key = names.iter().any(|n| n == "key");

        // Probes are generated against the template's literal text so they
        // cannot collide with it -- see `probe_prefix_for`.
        let literals: String = parts
            .iter()
            .filter_map(|p| match p {
                Part::Literal(s) => Some(s.as_str()),
                Part::Field(_) => None,
            })
            .collect();
        let prefix = probe_prefix_for(&literals);
        let probes: Vec<String> = (0..names.len()).map(|i| probe_for(&prefix, i)).collect();

        // Substitute inert stand-ins and see what kind of URL this actually is.
        let probe: String = parts
            .iter()
            .map(|p| match p {
                Part::Literal(s) => s.as_str(),
                Part::Field(i) => probes[*i].as_str(),
            })
            .collect();

        // `url::ParseError` describes the problem without echoing the input,
        // which is what makes it safe to surface here.
        let url = Url::parse(&probe).map_err(|e| {
            Error::msg(format!(
                "download_url_template is not a valid URL once its placeholders \
                 are filled in: {e}"
            ))
        })?;

        if !matches!(url.scheme(), "http" | "https") {
            return Err(Error::msg(format!(
                "download_url_template must be an http:// or https:// URL, not {}://",
                url.scheme()
            )));
        }
        if url.host_str().is_none_or(str::is_empty) {
            return Err(Error::msg(
                "download_url_template has no host; it must be an absolute URL",
            ));
        }

        // A placeholder in the authority cannot be made safe by escaping: the
        // value would choose the server the rss_key is sent to.
        let authority = authority_of(&url);
        for (i, p) in probes.iter().enumerate() {
            if authority.contains(p.as_str()) {
                return Err(Error::msg(format!(
                    "download_url_template puts {{{}}} in the host or port. \
                     Placeholders are filled from IRC messages, so they are only \
                     allowed in the path, query or fragment.",
                    names[i]
                )));
            }
        }

        Ok(Self { parts, names, authority, uses_key })
    }

    /// Whether the template mentions `{key}`, and so needs a non-empty `rss_key`.
    pub fn uses_key(&self) -> bool {
        self.uses_key
    }

    /// Distinct placeholder names, for the config layer to check against the
    /// announce regex's captures.
    ///
    /// That check is load-bearing rather than a convenience: this module can no
    /// longer tell an unknown placeholder from a valid one, because "valid"
    /// now depends on a regex it never sees. See `validate_platform`.
    pub fn placeholder_names(&self) -> &[String] {
        &self.names
    }

    pub fn render(&self, fields: &impl FieldSource) -> Result<String, Error> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Literal(s) => out.push_str(s),
                Part::Field(i) => {
                    let name = &self.names[*i];
                    // Hard error, never an empty substitution. The config
                    // cross-check should have made this unreachable, so getting
                    // here means that proof was bypassed -- and a refused
                    // download is a far better failure than a URL built with a
                    // hole in it.
                    let Some(raw) = fields.get(name) else {
                        return Err(Error::msg(format!(
                            "refusing to build a download URL: {{{name}}} has no value. \
                             It must name a capture that participated in the announce line."
                        )));
                    };
                    // `.` and `..` are the one thing escaping does not neutralise:
                    // they stay unreserved and a whole segment of ".." is removed
                    // by path normalisation, shifting everything after it. An
                    // empty value is the same hazard from the other direction --
                    // `/a/{x}/b` collapses to `/a//b`, a different path.
                    if raw.is_empty() {
                        return Err(Error::msg(format!(
                            "refusing to build a download URL: {{{name}}} is empty, which \
                             would collapse a path segment"
                        )));
                    }
                    if raw == "." || raw == ".." {
                        return Err(Error::msg(format!(
                            "refusing to build a download URL: {{{name}}} is {raw:?}"
                        )));
                    }
                    out.extend(utf8_percent_encode(raw, UNRESERVED));
                }
            }
        }

        // Redundant given the encoding above, and kept for the same reason
        // `assert_within` is kept next to the file write: if the encode set is
        // ever loosened, this still refuses.
        let url = Url::parse(&out)
            .map_err(|e| Error::msg(format!("built an unparseable download URL: {e}")))?;
        if authority_of(&url) != self.authority {
            return Err(Error::msg(
                "refusing to request a download from a different host than \
                 download_url_template names",
            ));
        }

        // The parsed form, not the string it was parsed from: what gets requested
        // is then exactly what was just checked, and any literal in the template
        // that needed escaping (a stray brace or space) is escaped rather than
        // left for reqwest to reinterpret.
        Ok(url.to_string())
    }
}


#[cfg(test)]
mod test {
    use super::*;
    use crate::template::MAX_NAME;

    const TPL: &str = "https://tracker.example.org/rss/download/{id}/{key}/{file}";

    /// A `FieldSource` backed by a plain map.
    ///
    /// Replaces the fixed four-field struct these tests used to build. The
    /// call sites below are unchanged on purpose: this module is the security
    /// spec for the download URL, so the generalisation has to leave every
    /// existing assertion passing verbatim.
    struct TestFields(std::collections::BTreeMap<String, String>);

    impl FieldSource for TestFields {
        fn get(&self, name: &str) -> Option<&str> {
            self.0.get(name).map(String::as_str)
        }
    }

    fn fields(id: &str, name: &str, file: &str, key: &str) -> TestFields {
        TestFields(
            [("id", id), ("name", name), ("file", file), ("key", key)]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    /// A source carrying only what is listed, for the missing-value paths.
    fn only(pairs: &[(&str, &str)]) -> TestFields {
        TestFields(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())
    }

    #[test]
    fn every_placeholder_is_substituted() {
        let t = UrlTemplate::parse("https://hypo.example/download/{id}/{file}?passkey={key}&n={name}")
            .unwrap();
        let url = t
            .render(&fields("8f3c1a2b", "Some.Release", "Some.Release.torrent", "abcd1234"))
            .unwrap();
        assert_eq!(
            url,
            "https://hypo.example/download/8f3c1a2b/Some.Release.torrent?passkey=abcd1234&n=Some.Release"
        );
    }

    #[test]
    fn uses_key_reports_whether_the_template_needs_one() {
        assert!(UrlTemplate::parse(TPL).unwrap().uses_key());
        assert!(!UrlTemplate::parse("https://h.example/d/{id}").unwrap().uses_key());
    }

    #[test]
    fn a_malformed_placeholder_name_is_rejected_at_parse() {
        // `{ids}` and `{Id}` used to be rejected here as "unknown"; they are now
        // syntactically fine and it is the config layer, which can see the
        // announce regex, that decides whether they name a real capture.
        // What this module still rejects is a name that could not *be* a
        // capture name.
        let e = UrlTemplate::parse("https://h.example/{}").unwrap_err().to_string();
        assert!(e.contains("empty placeholder"), "{e}");

        let e = UrlTemplate::parse("https://h.example/{a-b}").unwrap_err().to_string();
        assert!(e.contains("letters, digits and underscore"), "{e}");

        let e = UrlTemplate::parse("https://h.example/{ünïcode}").unwrap_err().to_string();
        assert!(e.contains("letters, digits and underscore"), "{e}");

        let long = "x".repeat(MAX_NAME + 1);
        let e = UrlTemplate::parse(&format!("https://h.example/{{{long}}}"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("longer than"), "{e}");
    }

    #[test]
    fn an_arbitrary_placeholder_name_is_accepted() {
        let t = UrlTemplate::parse("https://h.example/{uploader}/{id}").unwrap();
        assert_eq!(t.placeholder_names(), ["uploader", "id"]);

        let url = t
            .render(&only(&[("uploader", "j3rico"), ("id", "42")]))
            .unwrap();
        assert_eq!(url, "https://h.example/j3rico/42");
    }

    #[test]
    fn an_arbitrary_placeholder_cannot_sit_in_the_authority() {
        // The property the whole module exists for, now that names are open.
        let e = UrlTemplate::parse("https://{uploader}.example/d")
            .unwrap_err()
            .to_string();
        assert!(e.contains("host or port"), "{e}");
        assert!(e.contains("{uploader}"), "the error must name the placeholder: {e}");
    }

    #[test]
    fn names_sharing_a_prefix_are_attributed_correctly() {
        // Index-based probes with a terminator: without the trailing sentinel
        // `{a}`'s probe would be a substring of `{ab}`'s and this would blame
        // the wrong one.
        // Both in the path/query: accepted, and neither shadows the other.
        let t = UrlTemplate::parse("https://h.example/{a}?x={ab}").unwrap();
        assert_eq!(t.placeholder_names(), ["a", "ab"]);
        let url = t.render(&only(&[("a", "1"), ("ab", "2")])).unwrap();
        assert_eq!(url, "https://h.example/1?x=2");

        // Only the longer one is in the authority, and only it is blamed.
        let e = UrlTemplate::parse("https://{ab}.example/{a}").unwrap_err().to_string();
        assert!(e.contains("{ab}"), "{e}");
    }

    #[test]
    fn a_repeated_placeholder_is_interned_once() {
        let t = UrlTemplate::parse("https://h.example/{id}/x/{id}").unwrap();
        assert_eq!(t.placeholder_names(), ["id"]);
        let url = t.render(&only(&[("id", "7")])).unwrap();
        assert_eq!(url, "https://h.example/7/x/7");
    }

    #[test]
    fn a_probe_never_reaches_the_rendered_url() {
        let t = UrlTemplate::parse("https://h.example/{id}/{name}/{file}?k={key}").unwrap();
        let url = t.render(&fields("1", "n", "f", "k")).unwrap();
        assert!(!url.contains(PROBE_PREFIX), "{url}");
    }

    #[test]
    fn a_literal_that_looks_like_a_probe_does_not_break_attribution() {
        // The prefix is lengthened until it cannot collide with the literal, so
        // this template is accepted rather than falsely blamed.
        let t = UrlTemplate::parse("https://h.example/qqzzprobe0q/{id}").unwrap();
        let url = t.render(&only(&[("id", "7")])).unwrap();
        assert_eq!(url, "https://h.example/qqzzprobe0q/7");
    }

    #[test]
    fn a_placeholder_with_no_value_is_refused() {
        // Hard error, never an empty substitution: the config cross-check makes
        // this unreachable, so reaching it means that proof was bypassed.
        let t = UrlTemplate::parse("https://h.example/{id}/{freeleech}").unwrap();
        let e = t.render(&only(&[("id", "7")])).unwrap_err().to_string();
        assert!(e.contains("{freeleech}"), "{e}");
        assert!(e.contains("no value"), "{e}");
    }

    #[test]
    fn an_empty_value_is_refused() {
        // `/a/{x}/b` would collapse to `/a//b`, a different path. Also a latent
        // fix for the fixed fields: `(?P<name>.*)` can match empty.
        let t = UrlTemplate::parse("https://h.example/{id}/x").unwrap();
        let e = t.render(&only(&[("id", "")])).unwrap_err().to_string();
        assert!(e.contains("empty"), "{e}");
    }

    #[test]
    fn an_unterminated_placeholder_is_rejected() {
        let e = UrlTemplate::parse("https://h.example/{id").unwrap_err().to_string();
        assert!(e.contains("unterminated"), "{e}");
    }

    #[test]
    fn a_stray_closing_brace_is_rejected() {
        let e = UrlTemplate::parse("https://h.example/a}b").unwrap_err().to_string();
        assert!(e.contains("no matching"), "{e}");
    }

    #[test]
    fn doubled_braces_are_literal() {
        let t = UrlTemplate::parse("https://h.example/{{id}}/{id}").unwrap();
        let url = t.render(&fields("7", "n", "f", "k")).unwrap();
        assert_eq!(url, "https://h.example/%7Bid%7D/7");
    }

    #[test]
    fn an_empty_template_is_rejected() {
        for bad in ["", "   "] {
            let e = UrlTemplate::parse(bad).unwrap_err().to_string();
            assert!(e.contains("empty"), "{e}");
        }
    }

    #[test]
    fn a_non_http_scheme_is_rejected() {
        for bad in ["file:///etc/passwd", "ftp://h.example/{id}"] {
            let e = UrlTemplate::parse(bad).unwrap_err().to_string();
            assert!(e.contains("http://"), "{bad}: {e}");
        }
    }

    #[test]
    fn a_relative_template_is_rejected() {
        let e = UrlTemplate::parse("/rss/download/{id}").unwrap_err().to_string();
        assert!(e.contains("not a valid URL"), "{e}");
    }

    #[test]
    fn a_placeholder_in_the_authority_is_rejected_at_parse() {
        let e = UrlTemplate::parse("https://{id}.example.org/d").unwrap_err().to_string();
        assert!(e.contains("host or port"), "{e}");
        assert!(e.contains("{id}"), "{e}");
    }

    // The reason this module exists.
    #[test]
    fn an_announce_name_cannot_change_the_host() {
        let t = UrlTemplate::parse(TPL).unwrap();
        let url = t
            .render(&fields("1", "x", "@evil.example/x?a=b#c", "SUPERSECRET"))
            .unwrap();
        let parsed = Url::parse(&url).unwrap();
        assert_eq!(parsed.host_str(), Some("tracker.example.org"));
        // The whole hostile value survives only as one escaped path segment.
        let last = parsed.path().rsplit('/').next().unwrap();
        for delim in ['/', '?', '#', '@'] {
            assert!(!last.contains(delim), "{last} still contains {delim}");
        }
    }

    #[test]
    fn an_id_cannot_escape_its_path_segment() {
        let t = UrlTemplate::parse(TPL).unwrap();
        let url = t.render(&fields("../../admin", "n", "f", "k")).unwrap();
        let parsed = Url::parse(&url).unwrap();
        assert_eq!(parsed.host_str(), Some("tracker.example.org"));
        assert!(parsed.path().starts_with("/rss/download/"), "{}", parsed.path());
        assert!(!parsed.path().contains("/admin"), "{}", parsed.path());
    }

    #[test]
    fn a_dot_dot_segment_is_refused_outright() {
        let t = UrlTemplate::parse(TPL).unwrap();
        for bad in [".", ".."] {
            let e = t.render(&fields(bad, "n", "f", "k")).unwrap_err().to_string();
            assert!(e.contains("refusing"), "{bad}: {e}");
        }
    }

    #[test]
    fn the_error_never_contains_the_key() {
        let t = UrlTemplate::parse(TPL).unwrap();
        let e = t
            .render(&fields("..", "n", "f", "SUPERSECRET"))
            .unwrap_err()
            .to_string();
        assert!(!e.contains("SUPERSECRET"), "{e}");

        // The two render paths added with dynamic placeholders. Both name the
        // placeholder, and neither may quote a value -- the missing-value one
        // in particular runs while other fields, including the key, are in hand.
        let e = t
            .render(&only(&[("id", "1"), ("name", "n"), ("file", "f")]))
            .unwrap_err()
            .to_string();
        assert!(e.contains("{key}") && e.contains("no value"), "{e}");

        let e = t
            .render(&fields("", "n", "f", "SUPERSECRET"))
            .unwrap_err()
            .to_string();
        assert!(!e.contains("SUPERSECRET"), "{e}");
    }
}
