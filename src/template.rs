//! `{placeholder}` templating, shared by the download URL and the client
//! metadata templates.
//!
//! The lexer lives here rather than in `platforms::url_template` because two
//! things now parse the same syntax, and a second hand-rolled scanner over
//! attacker-influenced text is exactly the duplication worth not having.
//!
//! What differs is what happens after lexing:
//!
//!   * [`crate::platforms::url_template::UrlTemplate`] percent-encodes every
//!     value, refuses a missing or empty one, and re-checks the URL's authority.
//!     A value there chooses a request target, so the module is built to fail
//!     closed.
//!   * [`TextTemplate`] renders plain text and treats a missing field as empty.
//!     A qBittorrent tag that comes out blank is a cosmetic loss, not a
//!     security one -- but the *caller* still has to sanitise the result for
//!     whatever it is about to be embedded in.

use anyhow::Error;

/// Longest placeholder name accepted. Generous for a capture name, short
/// enough that an error message stays readable.
pub(crate) const MAX_NAME: usize = 64;

/// Where a template looks up a placeholder's value.
pub(crate) trait FieldSource {
    /// The value as captured. `None` if the field is not present.
    fn get(&self, name: &str) -> Option<&str>;

    /// The field's values, for a capture with `[captures.<n>].split` set.
    ///
    /// Defaults to the single raw value, which is what the URL side wants: a
    /// download URL takes one value per placeholder, never a list.
    fn values(&self, name: &str) -> Vec<&str> {
        self.get(name).into_iter().collect()
    }
}

#[derive(Debug)]
pub(crate) enum Part {
    Literal(String),
    /// Index into the template's `names`, so a name is stored once however
    /// often it is used.
    Field(usize),
}

/// Restrict names to what a regex capture name can be.
///
/// A subset of what `regex` accepts, which is what lets the config layer treat
/// "is this placeholder declared" as plain set containment rather than a fuzzy
/// comparison.
pub(crate) fn check_name(name: &str, what: &str) -> Result<(), Error> {
    if name.is_empty() {
        return Err(Error::msg(format!("{what} has an empty placeholder `{{}}`")));
    }
    if name.len() > MAX_NAME {
        return Err(Error::msg(format!(
            "{what} has a placeholder name longer than {MAX_NAME} characters"
        )));
    }
    if let Some(c) = name.chars().find(|c| !c.is_ascii_alphanumeric() && *c != '_') {
        return Err(Error::msg(format!(
            "{what} placeholder `{{{name}}}` contains {c:?}; names may use letters, digits and \
             underscore only"
        )));
    }
    Ok(())
}

/// Split a template into literals and placeholders.
///
/// `{{` and `}}` are literal braces, so a value that genuinely contains one is
/// still expressible. `what` names the setting in every error, since the same
/// lexer now serves several.
pub(crate) fn lex(template: &str, what: &str) -> Result<(Vec<Part>, Vec<String>), Error> {
    let mut parts = Vec::new();
    let mut names: Vec<String> = Vec::new();
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
                return Err(Error::msg(format!(
                    "{what} has a `}}` with no matching `{{`; write `}}}}` for a literal brace"
                )))
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
                        "{what} has an unterminated `{{{name}`"
                    )));
                }
                check_name(&name, what)?;
                let idx = match names.iter().position(|n| *n == name) {
                    Some(i) => i,
                    None => {
                        names.push(name);
                        names.len() - 1
                    }
                };
                if !literal.is_empty() {
                    parts.push(Part::Literal(std::mem::take(&mut literal)));
                }
                parts.push(Part::Field(idx));
            }
            _ => literal.push(c),
        }
    }

    if !literal.is_empty() {
        parts.push(Part::Literal(literal));
    }
    Ok((parts, names))
}

/// Separator for a placeholder whose field carries several values.
///
/// A comma because the one consumer is qBittorrent's `tags`, which is
/// comma-separated -- so a capture with `split = ","` set expands straight back
/// into the list it came from.
const JOIN: &str = ",";

/// A template rendered as plain text.
#[derive(Debug)]
pub(crate) struct TextTemplate {
    parts: Vec<Part>,
    names: Vec<String>,
}

impl TextTemplate {
    pub fn parse(template: &str, what: &str) -> Result<Self, Error> {
        let (parts, names) = lex(template, what)?;
        Ok(Self { parts, names })
    }

    /// Distinct placeholder names, for the config layer to check against the
    /// announce regex's captures.
    pub fn placeholder_names(&self) -> &[String] {
        &self.names
    }

    /// Substitute, treating an absent field as empty.
    ///
    /// Deliberately not an error, unlike the URL side: a release with no
    /// `uploader` capture should still be added, just without that tag. The
    /// caller sanitises the result -- this does not know what it is going into.
    pub fn render(&self, fields: &impl FieldSource) -> String {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Literal(s) => out.push_str(s),
                Part::Field(i) => {
                    let values = fields.values(&self.names[*i]);
                    out.push_str(&values.join(JOIN));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::collections::BTreeMap;

    struct Fields(BTreeMap<String, Vec<String>>);

    impl FieldSource for Fields {
        fn get(&self, name: &str) -> Option<&str> {
            self.0.get(name).and_then(|v| v.first()).map(String::as_str)
        }
        fn values(&self, name: &str) -> Vec<&str> {
            self.0.get(name).map(|v| v.iter().map(String::as_str).collect()).unwrap_or_default()
        }
    }

    fn fields(pairs: &[(&str, &[&str])]) -> Fields {
        Fields(
            pairs
                .iter()
                .map(|(k, vs)| {
                    ((*k).to_string(), vs.iter().map(|v| (*v).to_string()).collect())
                })
                .collect(),
        )
    }

    #[test]
    fn placeholders_are_substituted() {
        let t = TextTemplate::parse("{category}/{uploader}", "tags_template").unwrap();
        let out = t.render(&fields(&[("category", &["Movies"]), ("uploader", &["j3rico"])]));
        assert_eq!(out, "Movies/j3rico");
    }

    #[test]
    fn a_multi_valued_field_expands_comma_separated() {
        // Which is what makes `split` and qBittorrent's tag list line up.
        let t = TextTemplate::parse("{tags}", "tags_template").unwrap();
        let out = t.render(&fields(&[("tags", &["hd", "remux"])]));
        assert_eq!(out, "hd,remux");
    }

    #[test]
    fn an_absent_field_renders_empty_rather_than_failing() {
        let t = TextTemplate::parse("{category},{uploader}", "tags_template").unwrap();
        let out = t.render(&fields(&[("category", &["Movies"])]));
        assert_eq!(out, "Movies,");
    }

    #[test]
    fn literal_text_and_doubled_braces_survive() {
        let t = TextTemplate::parse("cat={{{category}}}", "tags_template").unwrap();
        let out = t.render(&fields(&[("category", &["Movies"])]));
        assert_eq!(out, "cat={Movies}");
    }

    #[test]
    fn a_malformed_name_is_rejected_and_the_error_names_the_setting() {
        let e = TextTemplate::parse("{a-b}", "tags_template").unwrap_err().to_string();
        assert!(e.contains("tags_template"), "{e}");
        assert!(e.contains("letters, digits and underscore"), "{e}");

        let e = TextTemplate::parse("{unterminated", "category_template")
            .unwrap_err()
            .to_string();
        assert!(e.contains("category_template") && e.contains("unterminated"), "{e}");
    }

    #[test]
    fn a_repeated_placeholder_is_interned_once() {
        let t = TextTemplate::parse("{a}-{a}", "tags_template").unwrap();
        assert_eq!(t.placeholder_names(), ["a"]);
        assert_eq!(t.render(&fields(&[("a", &["x"])])), "x-x");
    }
}
