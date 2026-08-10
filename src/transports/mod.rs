//! Chat transports that carry commands as well as notifications.
//!
//! IRC is not one of these: it lives in `irc_processor` because it is also the
//! announce source, and its constraints are nothing like these. A PRIVMSG is
//! capped at 512 bytes for the whole line and servers kill clients that send
//! several quickly, which is why that path needs a per-line budget, a pacer and
//! a cancellation epoch.
//!
//! Telegram and Discord have none of those problems: 4096 and 2000 characters
//! respectively, real newlines, and no flood limit worth pacing for at this
//! volume. A whole torrent listing fits in one message, so the work here is
//! packing logical lines into as few messages as the platform allows.

pub mod slack;
pub mod telegram;

/// Pack logical lines into as few messages as `budget` characters allows.
///
/// Counts `char`s, not bytes: these platforms limit by characters, and a torrent
/// name is arbitrary UTF-8, so measuring bytes would cut short messages that
/// were within the real limit.
///
/// A line is never split across messages -- a torrent's row staying intact is
/// worth more than filling every message exactly. The exception is a single line
/// longer than the whole budget, which is truncated on a char boundary because
/// there is nowhere else for it to go.
pub fn chunk_lines(lines: &[String], budget: usize) -> Vec<String> {
    let budget = budget.max(16);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();

    for line in lines {
        let line = if line.chars().count() > budget { truncate_chars(line, budget) } else { line.clone() };

        // +1 for the newline that would join it to what is already here.
        let would_be = current.chars().count() + 1 + line.chars().count();
        if !current.is_empty() && would_be > budget {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(&line);
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Pack a reply, and refuse to turn it into a hundred messages.
///
/// The line cap that IRC needs does not apply here, so a listing arrives whole
/// -- but "whole" has to stay bounded somewhere. A library of several thousand
/// torrents would otherwise chunk into a hundred-odd messages and earn a 429
/// from either platform, which is a worse answer than a truncated one.
///
/// Ten messages is roughly five hundred torrent rows: past any real reply, and
/// well short of a rate limit. What is left out is stated rather than dropped.
pub fn chunk_reply(lines: &[String], budget: usize, max_chunks: usize) -> Vec<String> {
    let mut chunks = chunk_lines(lines, budget);
    if chunks.len() <= max_chunks {
        return chunks;
    }

    // Count what the dropped chunks were carrying, so the note is about
    // torrents rather than about messages, which mean nothing to the reader.
    let kept: usize = chunks.iter().take(max_chunks).map(|c| c.lines().count()).sum();
    chunks.truncate(max_chunks);
    chunks.push(format!("… and {} more (not shown)", lines.len().saturating_sub(kept)));
    chunks
}

/// Cut to `budget` characters, marking that something was removed.
fn truncate_chars(s: &str, budget: usize) -> String {
    let keep = budget.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod test {
    use super::*;

    fn lines(n: usize, each: usize) -> Vec<String> {
        (0..n).map(|i| format!("{i:03}{}", "x".repeat(each.saturating_sub(3)))).collect()
    }

    #[test]
    fn everything_fitting_becomes_one_message() {
        let out = chunk_lines(&lines(5, 10), 4096);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].lines().count(), 5);
    }

    /// The point of the whole module: no message may exceed what the platform
    /// accepts, or it is rejected outright rather than truncated politely.
    #[test]
    fn no_chunk_exceeds_the_budget() {
        for budget in [20usize, 64, 100, 4096] {
            for chunk in chunk_lines(&lines(200, 30), budget) {
                assert!(
                    chunk.chars().count() <= budget,
                    "budget {budget}: chunk of {} chars",
                    chunk.chars().count()
                );
            }
        }
    }

    /// A torrent's row must arrive whole. Splitting one across two messages is
    /// worse than sending a slightly emptier message.
    #[test]
    fn a_line_is_never_split_across_chunks() {
        let input = lines(40, 25);
        let rejoined: Vec<String> =
            chunk_lines(&input, 100).iter().flat_map(|c| c.lines().map(str::to_string).collect::<Vec<_>>()).collect();

        assert_eq!(rejoined, input, "every line should survive intact and in order");
    }

    #[test]
    fn chunks_are_packed_greedily_rather_than_one_line_each() {
        // 10 lines of 9 chars into a 100-char budget is comfortably one message.
        assert_eq!(chunk_lines(&lines(10, 9), 100).len(), 1);
    }

    /// A name longer than a whole message has nowhere to go but the bin; cutting
    /// on a char boundary keeps it valid UTF-8 rather than panicking.
    #[test]
    fn an_overlong_line_is_truncated_on_a_char_boundary() {
        let out = chunk_lines(&[format!("{}", "é".repeat(500))], 50);
        assert_eq!(out.len(), 1);
        assert!(out[0].chars().count() <= 50, "{}", out[0].chars().count());
        assert!(out[0].ends_with('…'));
    }

    #[test]
    fn nothing_in_nothing_out() {
        assert!(chunk_lines(&[], 4096).is_empty());
    }

    /// A realistic listing must not be touched by the message guard.
    #[test]
    fn an_ordinary_reply_passes_the_message_guard_unchanged() {
        let input = lines(200, 80);
        let out = chunk_reply(&input, 3900, 10);

        assert_eq!(out, chunk_lines(&input, 3900));
        assert!(!out.last().unwrap().contains("not shown"));
    }

    /// A pathological one is bounded, and says what it left out -- a hundred
    /// messages would be rate-limited, which loses more than truncation does.
    #[test]
    fn a_pathological_reply_is_bounded_and_says_so() {
        let input = lines(5000, 80);
        let out = chunk_reply(&input, 3900, 10);

        assert_eq!(out.len(), 11, "ten messages plus the note");
        let note = out.last().unwrap();
        assert!(note.contains("not shown"), "{note}");

        // The number has to be the count of *lines* left out, not messages.
        let shown: usize = out[..10].iter().map(|c| c.lines().count()).sum();
        assert!(note.contains(&(5000 - shown).to_string()), "{note}");
    }
}
