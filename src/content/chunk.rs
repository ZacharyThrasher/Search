//! Splits extracted text into passage-sized chunks.
//!
//! A chunk keeps the byte range it came from (`start..end`, into the
//! concatenation of all segments of the document, one `\n` between segments)
//! and the page it starts on, so a later step (embeddings) can reuse the same
//! passages and a result can say where in the file the match was.

/// One page, slide or the whole text of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// 1-based page or slide number; 0 when the format has none.
    pub page: u32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub start: u64,
    pub end: u64,
    pub page: u32,
    /// The passage with whitespace collapsed to single spaces.
    pub text: String,
}

/// Most text kept from one file (the rest is not indexed).
pub const MAX_TEXT_BYTES: usize = 4 << 20;

/// Cut at a char boundary at or before `i`.
fn floor_boundary(t: &str, mut i: usize) -> usize {
    i = i.min(t.len());
    while !t.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Cut at a char boundary at or after `i`.
fn ceil_boundary(t: &str, mut i: usize) -> usize {
    while i < t.len() && !t.is_char_boundary(i) {
        i += 1;
    }
    i.min(t.len())
}

/// Where to end a chunk that starts at `pos` and should be about `target`
/// bytes: a paragraph break, else a sentence end, else a space, searching back
/// from the target first (so chunks are never much longer than asked).
fn find_break(t: &str, pos: usize, target: usize) -> usize {
    let b = t.as_bytes();
    let lo = pos + target * 6 / 10;
    let mid = (pos + target).min(b.len());
    let hi = (pos + target * 14 / 10).min(b.len());
    // Paragraph.
    let mut i = mid;
    while i > lo {
        if b[i - 1] == b'\n' && i >= 2 && b[i - 2] == b'\n' {
            return i;
        }
        i -= 1;
    }
    // Sentence.
    let mut i = mid;
    while i > lo {
        if matches!(b[i - 1], b' ' | b'\n' | b'\t') && i >= 2 && matches!(b[i - 2], b'.' | b'!' | b'?') {
            return i;
        }
        i -= 1;
    }
    // Any white space, back then forward.
    let mut i = mid;
    while i > lo {
        if b[i - 1].is_ascii_whitespace() {
            return i;
        }
        i -= 1;
    }
    let mut i = mid;
    while i < hi {
        if b[i].is_ascii_whitespace() {
            return i + 1;
        }
        i += 1;
    }
    ceil_boundary(t, mid)
}

/// Collapse every run of white space to one space and trim.
pub fn squash(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    for ch in s.chars() {
        if ch.is_whitespace() || ch == '\u{feff}' || ch == '\0' {
            gap = true;
        } else {
            if gap && !out.is_empty() {
                out.push(' ');
            }
            gap = false;
            out.push(ch);
        }
    }
    out
}

/// Split `segments` into chunks of about `target` bytes. The flag says the
/// text was cut at `MAX_TEXT_BYTES`.
pub fn chunk_segments(segments: &[Segment], target: usize) -> (Vec<Chunk>, bool) {
    let target = target.max(200);
    let mut chunks = Vec::new();
    let mut base = 0usize;
    let mut kept = 0usize;
    for seg in segments {
        let t = seg.text.as_str();
        let mut pos = 0;
        while pos < t.len() {
            // Skip leading white space (it still counts toward the offsets).
            let rest = &t[pos..];
            let lead = rest.len() - rest.trim_start().len();
            pos += lead;
            if pos >= t.len() {
                break;
            }
            let end = if t.len() - pos <= target * 13 / 10 {
                t.len()
            } else {
                find_break(t, pos, target).max(pos + 1)
            };
            let end = floor_boundary(t, end).max(ceil_boundary(t, pos + 1));
            let piece = squash(&t[pos..end]);
            if piece.chars().any(char::is_alphanumeric) {
                kept += piece.len();
                if kept > MAX_TEXT_BYTES {
                    return (chunks, true);
                }
                chunks.push(Chunk {
                    start: (base + pos) as u64,
                    end: (base + end) as u64,
                    page: seg.page,
                    text: piece,
                });
            }
            pos = end;
        }
        base += t.len() + 1;
    }
    (chunks, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str) -> Vec<Segment> {
        vec![Segment { page: 0, text: text.into() }]
    }

    #[test]
    fn short_text_is_one_chunk_with_offsets() {
        let (c, cut) = chunk_segments(&seg("  Hello   world.\n\nSecond line.  "), 1200);
        assert!(!cut);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].text, "Hello world. Second line.");
        assert_eq!(c[0].start, 2);
        assert_eq!(c[0].end as usize, "  Hello   world.\n\nSecond line.  ".len());
    }

    #[test]
    fn long_text_breaks_at_paragraphs_then_sentences_then_spaces() {
        let para = "word ".repeat(100); // 500 bytes
        let text = format!("{para}\n\n{para}\n\n{para}\n\n{para}");
        let (c, _) = chunk_segments(&seg(&text), 1100);
        assert!(c.len() >= 2);
        for ch in &c {
            assert!(ch.text.len() <= 1100 * 14 / 10, "{}", ch.text.len());
            assert!(!ch.text.starts_with(' ') && !ch.text.ends_with(' '));
        }
        // Chunks are contiguous and cover the text in order.
        for w in c.windows(2) {
            assert!(w[0].end <= w[1].start + 1, "{:?}", (w[0].end, w[1].start));
        }
        let joined: String = c.iter().map(|x| x.text.as_str()).collect::<Vec<_>>().join(" ");
        assert_eq!(joined, squash(&text));

        let sentences = "This is a sentence. ".repeat(200);
        let (c, _) = chunk_segments(&seg(&sentences), 500);
        assert!(c.iter().all(|ch| ch.text.ends_with('.')), "{:?}", c.iter().find(|ch| !ch.text.ends_with('.')));
    }

    #[test]
    fn a_word_longer_than_a_chunk_is_cut_without_panicking() {
        let blob = "x".repeat(10_000);
        let (c, _) = chunk_segments(&seg(&blob), 500);
        assert!(c.len() >= 10);
        assert_eq!(c.iter().map(|x| x.text.len()).sum::<usize>(), 10_000);
        // Multi-byte characters are never split.
        let wide = "é".repeat(5_000);
        let (c, _) = chunk_segments(&seg(&wide), 301);
        assert_eq!(c.iter().map(|x| x.text.chars().count()).sum::<usize>(), 5_000);
    }

    #[test]
    fn pages_and_offsets_span_segments() {
        let segs = vec![
            Segment { page: 1, text: "alpha".into() },
            Segment { page: 2, text: "beta".into() },
        ];
        let (c, _) = chunk_segments(&segs, 1200);
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].page, c[0].start, c[0].end), (1, 0, 5));
        assert_eq!((c[1].page, c[1].start, c[1].end), (2, 6, 10));
    }

    #[test]
    fn empty_and_symbol_only_text_makes_nothing() {
        assert!(chunk_segments(&seg(""), 1200).0.is_empty());
        assert!(chunk_segments(&seg("   \n\t "), 1200).0.is_empty());
        assert!(chunk_segments(&seg("---- ==== ****"), 1200).0.is_empty());
    }

    #[test]
    fn text_is_capped() {
        let big = "lorem ipsum dolor sit amet ".repeat(MAX_TEXT_BYTES / 20);
        let (c, cut) = chunk_segments(&seg(&big), 1200);
        assert!(cut);
        assert!(c.iter().map(|x| x.text.len()).sum::<usize>() <= MAX_TEXT_BYTES);
    }
}
