//! Removing mIRC formatting from an incoming message.
//!
//! Announce bots colour their output, and the codes are invisible in a log
//! while being very real to a regex. TorrentLeech sends:
//!
//! ```text
//! \x0300,04New Torrent Announcement:\x0300,12 <Movies :: 4K>  Name:'...'
//! ```
//!
//! `\x03` does not print, so a log shows `00,04New Torrent Announcement:00,12
//! <Movies :: 4K>` and the line looks like it should match. It does not:
//! `Announcement:` is followed by `\x03`, not by the space a pattern expects.
//! The only symptom is "Message is not a torrent or a command", which reads as
//! a wrong regex rather than an unprintable byte.
//!
//! Stripping here rather than asking every user to litter their pattern with
//! `\x03\d{1,2}(?:,\d{1,2})?` keeps `options.toml` describing the announce
//! *text*, which is the whole premise of having no tracker-specific code: a
//! network should be an announce regex and a URL template, not an exercise in
//! control-code escaping. It also protects the filter lists, which run against
//! a captured value that a tracker is free to colour from within.

use std::borrow::Cow;
use std::iter::Peekable;
use std::str::Chars;

/// Formatting codes that are a single character with no argument: bold, reset,
/// monospace, reverse, italic, strikethrough, underline.
const TOGGLES: [char; 7] = ['\u{2}', '\u{f}', '\u{11}', '\u{16}', '\u{1d}', '\u{1e}', '\u{1f}'];

/// Colour, `\x03[fg[,bg]]`, where each half is one or two digits.
const COLOUR: char = '\u{3}';

/// Hex colour, `\x04RRGGBB`. Rare, but cheap to handle and otherwise it leaks
/// six stray characters into the middle of a release name.
const HEX_COLOUR: char = '\u{4}';

/// Strip mIRC formatting, leaving the text a human sees.
///
/// Borrows when there is nothing to remove, which is the overwhelmingly common
/// case -- this runs on every PRIVMSG, including the channel chatter that is
/// neither an announcement nor a command.
pub(crate) fn strip_formatting(line: &str) -> Cow<'_, str> {
    if !line.contains(is_formatting) {
        return Cow::Borrowed(line);
    }

    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            COLOUR => {
                // A bare `\x03` resets colour and takes no argument, so the
                // digits are optional. The comma is only part of the code when
                // digits follow it -- `\x0304,text` is colour 04 and then a
                // literal `,text`, and eating that comma would corrupt a
                // release name.
                //
                // Where digits *do* follow, they are a background colour and go:
                // `\x0304,000` is fg 04 on bg 00 followed by `0`. That is
                // genuinely ambiguous in the format rather than a choice made
                // here, and every client resolves it the same way.
                if take_while(&mut chars, 2, char::is_ascii_digit) > 0
                    && chars.peek() == Some(&',')
                {
                    let mut lookahead = chars.clone();
                    lookahead.next();
                    if take_while(&mut lookahead, 2, char::is_ascii_digit) > 0 {
                        chars = lookahead;
                    }
                }
            }
            HEX_COLOUR => {
                take_while(&mut chars, 6, char::is_ascii_hexdigit);
            }
            c if TOGGLES.contains(&c) => {}
            c => out.push(c),
        }
    }

    Cow::Owned(out)
}

fn is_formatting(c: char) -> bool {
    c == COLOUR || c == HEX_COLOUR || TOGGLES.contains(&c)
}

/// Consume up to `max` characters satisfying `accept`, returning how many went.
fn take_while(chars: &mut Peekable<Chars<'_>>, max: usize, accept: fn(&char) -> bool) -> usize {
    let mut taken = 0;
    while taken < max {
        match chars.peek() {
            Some(c) if accept(c) => {
                chars.next();
                taken += 1;
            }
            _ => break,
        }
    }
    taken
}

#[cfg(test)]
mod test {
    use super::*;

    /// The line that prompted this module, byte for byte.
    const TORRENTLEECH: &str = "\u{3}00,04New Torrent Announcement:\u{3}00,12 <Movies :: 4K>  \
         Name:'28 Days Later 2002 2160p UHD BluRay H265-MALUS' uploaded by 'Anonymous' \
         freeleech - \u{3}01,15 https://www.torrentleech.org/torrent/241826790";

    #[test]
    fn a_real_torrentleech_announce_comes_out_as_plain_text() {
        assert_eq!(
            strip_formatting(TORRENTLEECH),
            "New Torrent Announcement: <Movies :: 4K>  \
             Name:'28 Days Later 2002 2160p UHD BluRay H265-MALUS' uploaded by 'Anonymous' \
             freeleech -  https://www.torrentleech.org/torrent/241826790"
        );
    }

    #[test]
    fn an_uncoloured_line_is_borrowed_not_copied() {
        // Every PRIVMSG passes through here, most carrying no formatting at all.
        let plain = "New Torrent Announcement: <Movies :: 4K>  Name:'x' uploaded by 'y' - http://t/torrent/1";
        assert!(matches!(strip_formatting(plain), Cow::Borrowed(_)));
    }

    #[test]
    fn both_halves_of_a_colour_code_go() {
        assert_eq!(strip_formatting("\u{3}00,04red\u{3}"), "red");
        assert_eq!(strip_formatting("\u{3}4one digit"), "one digit");
        assert_eq!(strip_formatting("\u{3}04,15both"), "both");
    }

    #[test]
    fn a_comma_is_only_eaten_when_digits_follow_it() {
        // No background to read, so the comma is content. Eating it would
        // corrupt the name the download filters then run against.
        assert_eq!(strip_formatting("\u{3}04,text"), ",text");
        // A bare reset has no foreground digits either, so there is no pair for
        // the comma to complete.
        assert_eq!(strip_formatting("\u{3},000 Leagues"), ",000 Leagues");
        // But digits after the comma really are a background colour, so they
        // go and the third zero is text. Ambiguous in the format itself -- a
        // release name that opens with a coloured thousands separator cannot be
        // told apart from `fg,bg`, and every client reads it this way.
        assert_eq!(strip_formatting("\u{3}04,000 Leagues"), "0 Leagues");
    }

    #[test]
    fn the_single_character_toggles_go_too() {
        assert_eq!(strip_formatting("\u{2}bold\u{2} \u{1f}under\u{1f} \u{1d}it\u{1d}"), "bold under it");
        assert_eq!(strip_formatting("\u{4}FF00AAhex"), "hex");
    }

    #[test]
    fn text_that_merely_looks_like_a_code_is_untouched() {
        // No control character, so nothing here is a colour code -- a release
        // name is free to contain digits, commas and the digit 3.
        let name = "Ocean's 3,000 2160p x265-04,15";
        assert_eq!(strip_formatting(name), name);
        assert!(matches!(strip_formatting(name), Cow::Borrowed(_)));
    }

    #[test]
    fn multibyte_text_survives() {
        // Release names carry en-dashes and worse; a byte-wise stripper would
        // slice one in half.
        assert_eq!(
            strip_formatting("\u{3}04The Lord of the Rings – Legacy Edition\u{f}"),
            "The Lord of the Rings – Legacy Edition"
        );
    }

    #[test]
    fn a_credential_split_by_a_colour_code_is_rejoined() {
        // `redact_secrets` matches `auth:[...]` literally, so a code between
        // `auth:` and `[` would defeat it and put the password in the log.
        // Stripping first is what makes the redactor see one token.
        assert_eq!(strip_formatting("auth:\u{3}04[hunter2]"), "auth:[hunter2]");
    }
}
