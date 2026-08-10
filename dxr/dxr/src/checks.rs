#![allow(clippy::unwrap_used)]

mod types;
mod values;

/// Whether a string can survive a round trip through XML text at all.
///
/// The string round-trip properties generate arbitrary `String`s, but XML text
/// cannot carry every `char`:
///
///   * XML 1.0 has no representation for most C0 control characters -- not even
///     as a numeric reference, since `&#0;` is itself ill-formed. Only tab, line
///     feed and carriage return are permitted at all.
///   * A literal carriage return *is* permitted but still does not survive:
///     parsers must normalise CR and CRLF to a single LF before the application
///     sees them (XML 1.0 s2.11), so `\r` never comes back as `\r`.
///
/// Such inputs are discarded rather than asserted on. quick-xml 0.30 happened to
/// round-trip some of them through its own lenient parser, which is the only
/// reason these properties passed there; it was never XML they should have
/// required.
pub(crate) fn is_xml_representable(s: &str) -> bool {
    !s.chars().any(|c| c.is_control() && c != '\t' && c != '\n')
}
