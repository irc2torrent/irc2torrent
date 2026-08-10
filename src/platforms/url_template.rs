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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Id,
    Name,
    File,
    Key,
}

impl Field {
    fn from_name(s: &str) -> Option<Self> {
        match s {
            "id" => Some(Self::Id),
            "name" => Some(Self::Name),
            "file" => Some(Self::File),
            "key" => Some(Self::Key),
            _ => None,
        }
    }

    /// A stand-in used only while validating the template.
    ///
    /// Alphanumeric, so it survives both URL parsing and `UNRESERVED` encoding
    /// unchanged -- if one of these turns up in the authority of the probe URL,
    /// the placeholder it came from is in a position substitution can never be
    /// made safe in.
    fn probe(self) -> &'static str {
        match self {
            Self::Id => "qqzzprobeid",
            Self::Name => "qqzzprobename",
            Self::File => "qqzzprobefile",
            Self::Key => "qqzzprobekey",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Name => "name",
            Self::File => "file",
            Self::Key => "key",
        }
    }
}

const VALID_PLACEHOLDERS: &str = "{id}, {name}, {file}, {key}";

#[derive(Debug)]
enum Part {
    Literal(String),
    Field(Field),
}

/// The values substituted for one download.
pub(crate) struct Fields<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub file: &'a str,
    pub key: &'a str,
}

impl Fields<'_> {
    fn get(&self, f: Field) -> &str {
        match f {
            Field::Id => self.id,
            Field::Name => self.name,
            Field::File => self.file,
            Field::Key => self.key,
        }
    }
}

#[derive(Debug)]
pub(crate) struct UrlTemplate {
    parts: Vec<Part>,
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
            return Err(Error::msg(
                "download_url_template is empty; set it to the URL your tracker \
                 serves .torrent files from, using the placeholders "
                    .to_string()
                    + VALID_PLACEHOLDERS,
            ));
        }

        let parts = lex(template)?;
        let uses_key = parts
            .iter()
            .any(|p| matches!(p, Part::Field(Field::Key)));

        // Substitute inert stand-ins and see what kind of URL this actually is.
        let probe: String = parts
            .iter()
            .map(|p| match p {
                Part::Literal(s) => s.as_str(),
                Part::Field(f) => f.probe(),
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
        for f in [Field::Id, Field::Name, Field::File, Field::Key] {
            if authority.contains(f.probe()) {
                return Err(Error::msg(format!(
                    "download_url_template puts {{{}}} in the host or port. \
                     Placeholders are filled from IRC messages, so they are only \
                     allowed in the path, query or fragment.",
                    f.as_str()
                )));
            }
        }

        Ok(Self { parts, authority, uses_key })
    }

    /// Whether the template mentions `{key}`, and so needs a non-empty `rss_key`.
    pub fn uses_key(&self) -> bool {
        self.uses_key
    }

    pub fn render(&self, fields: Fields<'_>) -> Result<String, Error> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Literal(s) => out.push_str(s),
                Part::Field(f) => {
                    let raw = fields.get(*f);
                    // `.` and `..` are the one thing escaping does not neutralise:
                    // they stay unreserved and a whole segment of ".." is removed
                    // by path normalisation, shifting everything after it.
                    if raw == "." || raw == ".." {
                        return Err(Error::msg(format!(
                            "refusing to build a download URL: {{{}}} is {:?}",
                            f.as_str(),
                            raw
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

/// Split a template into literals and placeholders.
///
/// `{{` and `}}` are literal braces, so a tracker whose URLs genuinely contain
/// one is still expressible.
fn lex(template: &str) -> Result<Vec<Part>, Error> {
    let mut parts = Vec::new();
    let mut literal = String::new();
    let mut chars = template.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                literal.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                literal.push('}');
            }
            '}' => {
                return Err(Error::msg(
                    "download_url_template has a `}` with no matching `{`; \
                     write `}}` for a literal brace",
                ))
            }
            '{' => {
                let mut name = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    name.push(c);
                }
                if !closed {
                    return Err(Error::msg(format!(
                        "download_url_template has an unterminated `{{{name}`; \
                         valid placeholders are {VALID_PLACEHOLDERS}"
                    )));
                }
                let field = Field::from_name(&name).ok_or_else(|| {
                    Error::msg(format!(
                        "download_url_template uses an unknown placeholder \
                         `{{{name}}}`; valid placeholders are {VALID_PLACEHOLDERS}"
                    ))
                })?;
                if !literal.is_empty() {
                    parts.push(Part::Literal(std::mem::take(&mut literal)));
                }
                parts.push(Part::Field(field));
            }
            _ => literal.push(c),
        }
    }

    if !literal.is_empty() {
        parts.push(Part::Literal(literal));
    }
    Ok(parts)
}

#[cfg(test)]
mod test {
    use super::*;

    const TPL: &str = "https://tracker.example.org/rss/download/{id}/{key}/{file}";

    fn fields<'a>(id: &'a str, name: &'a str, file: &'a str, key: &'a str) -> Fields<'a> {
        Fields { id, name, file, key }
    }

    #[test]
    fn every_placeholder_is_substituted() {
        let t = UrlTemplate::parse("https://hypo.example/download/{id}/{file}?passkey={key}&n={name}")
            .unwrap();
        let url = t
            .render(fields("8f3c1a2b", "Some.Release", "Some.Release.torrent", "abcd1234"))
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
    fn an_unknown_placeholder_is_rejected_at_parse() {
        for bad in ["https://h.example/{ids}", "https://h.example/{Id}", "https://h.example/{}"] {
            let e = UrlTemplate::parse(bad).unwrap_err().to_string();
            assert!(e.contains("unknown placeholder"), "{bad}: {e}");
            assert!(e.contains("{id}"), "{bad}: {e}");
        }
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
        let url = t.render(fields("7", "n", "f", "k")).unwrap();
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
            .render(fields("1", "x", "@evil.example/x?a=b#c", "SUPERSECRET"))
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
        let url = t.render(fields("../../admin", "n", "f", "k")).unwrap();
        let parsed = Url::parse(&url).unwrap();
        assert_eq!(parsed.host_str(), Some("tracker.example.org"));
        assert!(parsed.path().starts_with("/rss/download/"), "{}", parsed.path());
        assert!(!parsed.path().contains("/admin"), "{}", parsed.path());
    }

    #[test]
    fn a_dot_dot_segment_is_refused_outright() {
        let t = UrlTemplate::parse(TPL).unwrap();
        for bad in [".", ".."] {
            let e = t.render(fields(bad, "n", "f", "k")).unwrap_err().to_string();
            assert!(e.contains("refusing"), "{bad}: {e}");
        }
    }

    #[test]
    fn the_error_never_contains_the_key() {
        let t = UrlTemplate::parse(TPL).unwrap();
        let e = t
            .render(fields("..", "n", "f", "SUPERSECRET"))
            .unwrap_err()
            .to_string();
        assert!(!e.contains("SUPERSECRET"), "{e}");
    }
}
