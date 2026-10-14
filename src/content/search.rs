//! Content queries: the reader side of the index (used by the popup and the
//! CLI), BM25 over passages, one hit per file with a one-line snippet.
//!
//! The query language is deliberately small. Words must all appear in the
//! same passage (if nothing matches that way, a looser pass that needs only
//! some of the words runs instead). `"in quotes"` is an exact phrase. The word being
//! typed (the last one, when the text does not end in a space) also matches
//! as a prefix, so results appear while you type.

use std::collections::HashSet;
use std::ops::Range;
use std::path::Path;
use std::time::Instant;

use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, PhraseQuery, Query, RegexQuery, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument, Term};

use super::store::{self, Fields, TOKENIZER, TOKENIZER_RAW};

/// Shortest word that is also matched as a prefix.
pub const MIN_PREFIX: usize = 3;
const SNIPPET_BYTES: usize = 180;

/// Whether `text` is worth searching file contents for. The name search has its
/// own syntax (wildcards `*` `?`, `|`, `!`, `ext:`, `regex:`, paths with a
/// slash); a query that uses it is about names, so no content search runs.
pub fn is_content_query(text: &str) -> bool {
    if text.chars().any(|c| matches!(c, '*' | '?' | '|' | '\\' | '/')) {
        return false;
    }
    !text.split_whitespace().any(|w| {
        let w = w.trim_start_matches('"').to_ascii_lowercase();
        w.starts_with('!') || w.starts_with("ext:") || w.starts_with("regex:") || w.starts_with("path:")
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentHit {
    pub path: String,
    pub score: f32,
    /// One line around the best match, `…` where cut.
    pub snippet: String,
    /// Byte ranges of `snippet` to emphasise.
    pub highlights: Vec<Range<usize>>,
    /// Where the passage starts in the file's extracted text.
    pub start: u64,
    /// Page or slide of the passage (0: none).
    pub page: u32,
}

#[derive(Debug, Default, Clone)]
pub struct Parsed {
    /// Stems that must appear (single words).
    stems: Vec<String>,
    /// Exact phrases, as stems.
    phrases: Vec<Vec<String>>,
    /// The word being typed, lower-cased and folded.
    prefix: Option<String>,
}

impl Parsed {
    pub fn is_empty(&self) -> bool {
        self.stems.is_empty() && self.phrases.is_empty() && self.prefix.is_none()
    }

    fn all_stems(&self) -> HashSet<&str> {
        self.stems
            .iter()
            .map(String::as_str)
            .chain(self.phrases.iter().flatten().map(String::as_str))
            .collect()
    }
}

fn tokens(a: &mut TextAnalyzer, text: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut ts = a.token_stream(text);
    while ts.advance() {
        let t = ts.token();
        out.push((t.text.clone(), t.offset_from, t.offset_to));
    }
    out
}

/// Split the user's text into words, quoted phrases and the prefix word.
pub fn parse(index: &Index, text: &str) -> Parsed {
    let Some(mut stem) = index.tokenizers().get(TOKENIZER) else { return Parsed::default() };
    let Some(mut raw) = index.tokenizers().get(TOKENIZER_RAW) else { return Parsed::default() };
    let mut p = Parsed::default();
    let ends_open = text.chars().last().is_some_and(|c| !c.is_whitespace() && c != '"');
    // Quoted phrases first; the rest of the text is plain words.
    let mut plain = String::new();
    let mut in_q = false;
    let mut cur = String::new();
    for ch in text.chars() {
        if ch == '"' {
            if in_q {
                let words: Vec<String> = tokens(&mut stem, &cur).into_iter().map(|t| t.0).collect();
                match words.len() {
                    0 => {}
                    1 => p.stems.push(words[0].clone()),
                    _ => p.phrases.push(words),
                }
                cur.clear();
            }
            in_q = !in_q;
            plain.push(' ');
        } else if in_q {
            cur.push(ch);
        } else {
            plain.push(ch);
        }
    }
    if in_q && !cur.is_empty() {
        // An unfinished quote: treat what is in it as words.
        plain.push(' ');
        plain.push_str(&cur);
    }
    let pieces: Vec<&str> = plain.split_whitespace().collect();
    let last_idx = pieces.len().checked_sub(1);
    for (i, piece) in pieces.iter().enumerate() {
        let words: Vec<String> = tokens(&mut stem, piece).into_iter().map(|t| t.0).collect();
        if words.is_empty() {
            continue;
        }
        let is_typing = ends_open && Some(i) == last_idx && !in_q;
        if is_typing {
            let r = tokens(&mut raw, piece);
            if let Some((w, ..)) = r.last()
                && w.chars().count() >= MIN_PREFIX
                && words.len() == 1
            {
                p.prefix = Some(w.clone());
                // The complete word also counts (handled in `build`).
                p.stems.push(words[0].clone());
                continue;
            }
            if words.len() == 1 && words[0].chars().count() < MIN_PREFIX {
                continue; // too short to mean anything yet
            }
        }
        match words.len() {
            1 => p.stems.push(words[0].clone()),
            _ => p.phrases.push(words),
        }
    }
    p
}

fn term(f: &Fields, w: &str) -> Term {
    Term::from_field_text(f.body, w)
}

fn word_query(f: &Fields, w: &str, prefix: Option<&str>) -> Box<dyn Query> {
    let exact: Box<dyn Query> = Box::new(TermQuery::new(term(f, w), IndexRecordOption::WithFreqs));
    match prefix.and_then(|p| RegexQuery::from_pattern(&format!("{}.*", store::regex_escape(p)), f.body).ok()) {
        Some(rx) => Box::new(BooleanQuery::new(vec![(Occur::Should, exact), (Occur::Should, Box::new(rx))])),
        None => exact,
    }
}

/// `all`: every word must appear; otherwise any of them.
fn build(f: &Fields, p: &Parsed, all: bool) -> Option<Box<dyn Query>> {
    let occur = if all { Occur::Must } else { Occur::Should };
    let mut parts: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for w in &p.stems {
        // The word being typed gets its prefix alternatives; its stem alone is pushed too.
        let is_prefix_word = p.prefix.is_some() && Some(w) == p.stems.last();
        parts.push((occur, word_query(f, w, if is_prefix_word { p.prefix.as_deref() } else { None })));
    }
    if p.stems.is_empty()
        && let Some(pre) = &p.prefix
        && let Ok(rx) = RegexQuery::from_pattern(&format!("{}.*", store::regex_escape(pre)), f.body)
    {
        parts.push((occur, Box::new(rx)));
    }
    for ph in &p.phrases {
        parts.push((occur, Box::new(PhraseQuery::new(ph.iter().map(|w| term(f, w)).collect()))));
    }
    match parts.len() {
        0 => None,
        1 => Some(parts.pop().unwrap().1),
        _ => Some(Box::new(BooleanQuery::new(parts))),
    }
}

/// Byte ranges in `body` that match the query (stems, phrases, prefix).
fn matches_in(index: &Index, body: &str, p: &Parsed) -> Vec<Range<usize>> {
    let mut marks: Vec<Range<usize>> = Vec::new();
    if let Some(mut stem) = index.tokenizers().get(TOKENIZER) {
        let want = p.all_stems();
        for (w, a, b) in tokens(&mut stem, body) {
            if want.contains(w.as_str()) {
                marks.push(a..b);
            }
        }
    }
    if let Some(pre) = &p.prefix
        && let Some(mut raw) = index.tokenizers().get(TOKENIZER_RAW)
    {
        for (w, a, b) in tokens(&mut raw, body) {
            if w.starts_with(pre.as_str()) {
                marks.push(a..b);
            }
        }
    }
    marks.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for m in marks {
        match merged.last_mut() {
            Some(l) if m.start <= l.end => l.end = l.end.max(m.end),
            _ => merged.push(m),
        }
    }
    merged
}

/// A one-line excerpt of `body` around the densest cluster of `marks`.
pub fn excerpt(body: &str, marks: &[Range<usize>], max: usize) -> (String, Vec<Range<usize>>) {
    let floor = |mut i: usize| {
        while i > 0 && !body.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let ceil = |mut i: usize| {
        while i < body.len() && !body.is_char_boundary(i) {
            i += 1;
        }
        i.min(body.len())
    };
    if body.len() <= max {
        return (body.to_string(), marks.to_vec());
    }
    // The window (of `max` bytes) that holds the most marks, starting at a mark.
    let mut best = (0usize, 0usize); // (count, start)
    for (i, m) in marks.iter().enumerate() {
        let n = marks[i..].iter().take_while(|x| x.end <= m.start + max - 20).count();
        if n > best.0 {
            best = (n, m.start);
        }
    }
    let lead = max / 5;
    let mut start = floor(best.1.saturating_sub(lead));
    // Begin at a word boundary.
    if start > 0
        && let Some(sp) = body[start..].find(' ')
        && start + sp < best.1
    {
        start += sp + 1;
    }
    let mut end = ceil((start + max).min(body.len()));
    if end < body.len()
        && let Some(sp) = body[..end].rfind(' ')
        && sp > start + max / 2
    {
        end = sp;
    }
    let mut s = String::new();
    let mut shift = 0usize;
    if start > 0 {
        s.push('…');
        shift = '…'.len_utf8();
    }
    s.push_str(&body[start..end]);
    if end < body.len() {
        s.push('…');
    }
    let hl = marks
        .iter()
        .filter(|m| m.start >= start && m.end <= end)
        .map(|m| m.start - start + shift..m.end - start + shift)
        .collect();
    (s, hl)
}

pub struct ContentReader {
    index: Index,
    reader: IndexReader,
    f: Fields,
    loaded: Instant,
}

impl ContentReader {
    pub fn open(dir: &Path) -> tantivy::Result<ContentReader> {
        let (index, f) = store::open_for_read(dir)?;
        let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;
        Ok(ContentReader { index, reader, f, loaded: Instant::now() })
    }

    /// Pick up commits the indexer made since the last reload.
    pub fn reload(&mut self) {
        // The indexer may be removing old segment files at this very moment;
        // a failed reload just keeps the previous view.
        if self.reader.reload().is_ok() {
            self.loaded = Instant::now();
        }
    }

    pub fn age(&self) -> std::time::Duration {
        self.loaded.elapsed()
    }

    /// Passages in the index (not files).
    pub fn passages(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    fn run(&self, q: &dyn Query, n: usize) -> Vec<(f32, tantivy::DocAddress)> {
        self.reader.searcher().search(q, &TopDocs::with_limit(n).order_by_score()).unwrap_or_default()
    }

    /// Best passage per file, best files first.
    pub fn search(&self, text: &str, limit: usize) -> Vec<ContentHit> {
        let p = parse(&self.index, text);
        if p.is_empty() || limit == 0 {
            return Vec::new();
        }
        let searcher = self.reader.searcher();
        let mut hits: Vec<ContentHit> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let multi = p.stems.len() + p.phrases.len() > 1;
        for pass in 0..2 {
            if pass == 1 && (!multi || !hits.is_empty()) {
                break;
            }
            let Some(q) = build(&self.f, &p, pass == 0) else { break };
            for (score, addr) in self.run(&*q, limit * 6) {
                let Ok(doc) = searcher.doc::<TantivyDocument>(addr) else { continue };
                let Some(path) = doc.get_first(self.f.path).and_then(|v| v.as_str()) else { continue };
                if !seen.insert(path.to_string()) {
                    continue;
                }
                let body = doc.get_first(self.f.body).and_then(|v| v.as_str()).unwrap_or("");
                let marks = matches_in(&self.index, body, &p);
                let (snippet, highlights) = excerpt(body, &marks, SNIPPET_BYTES);
                hits.push(ContentHit {
                    path: path.to_string(),
                    // Looser-pass hits rank below every strict one.
                    score: if pass == 0 { score } else { score * 0.01 },
                    snippet,
                    highlights,
                    start: doc.get_first(self.f.start).and_then(|v| v.as_u64()).unwrap_or(0),
                    page: doc.get_first(self.f.page).and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                });
                if hits.len() >= limit {
                    break;
                }
            }
            if hits.len() >= limit {
                break;
            }
        }
        hits
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;
    use crate::content::chunk::Chunk;
    use crate::content::extract::Status;
    use crate::content::store::{FileDoc, Writer};

    fn doc(path: &str, texts: &[&str]) -> FileDoc {
        FileDoc {
            path: path.into(),
            size: 1,
            mtime: 1,
            status: Status::Indexed,
            chunks: texts
                .iter()
                .enumerate()
                .map(|(i, t)| Chunk { start: i as u64 * 1000, end: i as u64 * 1000 + t.len() as u64, page: i as u32 + 1, text: (*t).into() })
                .collect(),
        }
    }

    fn corpus() -> (tempfile::TempDir, ContentReader) {
        let dir = tempfile::tempdir().unwrap();
        let (mut w, _) = Writer::open(dir.path(), 15_000_000).unwrap();
        w.put(doc(
            r"C:\docs\report.pdf",
            &[
                "The quarterly report shows revenue growth across all regions.",
                "Appendix: methodology, sampling and the margin of error.",
            ],
        ))
        .unwrap();
        w.put(doc(r"C:\docs\notes.txt", &["Remember to send the invoice to Acme about the shipment of reports."])).unwrap();
        w.put(doc(r"C:\code\main.rs", &["fn parse_config(path: &str) -> Config { read_to_string(path) }"])).unwrap();
        w.put(doc(r"C:\docs\cafe.md", &["We met at the café on Monday to discuss the budget."])).unwrap();
        w.commit().unwrap();
        w.finish().unwrap();
        let r = ContentReader::open(dir.path()).unwrap();
        (dir, r)
    }

    fn paths(h: &[ContentHit]) -> Vec<&str> {
        h.iter().map(|x| x.path.as_str()).collect()
    }

    #[test]
    fn name_syntax_is_not_a_content_query() {
        for yes in ["quarterly report", "\"exact phrase\"", "snake_case", "don't", "a-b", "c++"] {
            assert!(is_content_query(yes), "{yes}");
        }
        for no in ["*.png", "report*", "a|b", "ext:png;jpg", "EXT:png", "regex:(", r"c:\users\x", "src/main", "!draft report", "?ab", "path:foo"] {
            assert!(!is_content_query(no), "{no}");
        }
    }

    #[test]
    fn finds_words_with_stemming_and_accents() {
        let (_d, r) = corpus();
        // "reports" stems to "report": both files match.
        let h = r.search("report ", 10);
        assert_eq!(h.len(), 2, "{h:?}");
        assert!(paths(&h).contains(&r"C:\docs\report.pdf") && paths(&h).contains(&r"C:\docs\notes.txt"));
        // Accents fold both ways.
        assert_eq!(paths(&r.search("cafe ", 10)), [r"C:\docs\cafe.md"]);
        assert_eq!(paths(&r.search("café ", 10)), [r"C:\docs\cafe.md"]);
        // Identifiers split at punctuation and match as a phrase.
        assert_eq!(paths(&r.search("parse_config ", 10)), [r"C:\code\main.rs"]);
        assert!(r.search("nonexistentword ", 10).is_empty());
    }

    #[test]
    fn all_words_must_share_a_passage_but_a_looser_pass_fills_in() {
        let (_d, r) = corpus();
        let h = r.search("invoice acme ", 10);
        assert_eq!(paths(&h), [r"C:\docs\notes.txt"]);
        // "revenue" and "methodology" are in different passages of one file.
        let h = r.search("revenue methodology ", 10);
        assert_eq!(paths(&h), [r"C:\docs\report.pdf"]);
        assert!(h[0].score > 0.0);
        // The looser pass only runs when the strict one found nothing: here
        // "report" and "invoice" share the notes passage.
        let h = r.search("report invoice ", 10);
        assert_eq!(paths(&h), [r"C:\docs\notes.txt"]);
        // No passage has both, so the loose pass lists files with either.
        let h = r.search("revenue acme ", 10);
        assert_eq!(h.len(), 2, "{h:?}");
    }

    #[test]
    fn phrases_and_the_word_being_typed() {
        let (_d, r) = corpus();
        assert_eq!(paths(&r.search("\"revenue growth\" ", 10)), [r"C:\docs\report.pdf"]);
        assert!(r.search("\"growth revenue\" ", 10).is_empty());
        // No trailing space: the last word is still being typed.
        assert_eq!(paths(&r.search("quarterly rep", 10)), [r"C:\docs\report.pdf"]);
        assert_eq!(paths(&r.search("sampl", 10)), [r"C:\docs\report.pdf"]);
        // Two letters mean nothing yet.
        assert!(r.search("sa", 10).is_empty());
        // The finished word works the same with or without a space.
        assert_eq!(r.search("revenue", 10).len(), 1);
    }

    #[test]
    fn hits_carry_a_marked_one_line_snippet_and_location() {
        let (_d, r) = corpus();
        let h = r.search("methodology ", 10);
        assert_eq!(h.len(), 1);
        let hit = &h[0];
        assert_eq!((hit.start, hit.page), (1000, 2));
        assert!(!hit.snippet.contains('\n'));
        let marked: Vec<&str> = hit.highlights.iter().map(|m| &hit.snippet[m.clone()]).collect();
        assert_eq!(marked, ["methodology"]);
        // Prefix matches are marked too.
        let h = r.search("sampl", 10);
        let marked: Vec<&str> = h[0].highlights.iter().map(|m| &h[0].snippet[m.clone()]).collect();
        assert_eq!(marked, ["sampling"]);
    }

    #[test]
    fn excerpts_are_cut_on_word_boundaries_around_the_match() {
        let body = format!("{} needle in the middle {}", "alpha ".repeat(60), "omega ".repeat(60));
        let at = body.find("needle").unwrap();
        let (s, hl) = excerpt(&body, &[at..at + 6], 120);
        assert!(s.starts_with('…') && s.ends_with('…'));
        assert!(s.len() <= 120 + 8);
        assert_eq!(&s[hl[0].clone()], "needle");
        let (s, hl) = excerpt("short text here", &[6..10], 120);
        assert_eq!((s.as_str(), hl), ("short text here", vec![6..10]));
        // Never splits a multi-byte character.
        let wide = format!("{}é match é{}", "ü".repeat(200), "ö".repeat(200));
        let at = wide.find("match").unwrap();
        let (s, hl) = excerpt(&wide, &[at..at + 5], 100);
        assert_eq!(&s[hl[0].clone()], "match");
    }

    #[test]
    fn a_reader_sees_new_commits_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        let (mut w, _) = Writer::open(dir.path(), 15_000_000).unwrap();
        w.put(doc(r"C:\a.txt", &["first document about zebras"])).unwrap();
        w.commit().unwrap();
        let mut r = ContentReader::open(dir.path()).unwrap();
        assert_eq!(r.search("zebras ", 5).len(), 1);
        w.put(doc(r"C:\b.txt", &["second document about zebras and llamas"])).unwrap();
        w.commit().unwrap();
        assert_eq!(r.search("zebras ", 5).len(), 1); // not yet reloaded
        r.reload();
        assert_eq!(r.search("zebras ", 5).len(), 2);
        w.remove(r"C:\a.txt");
        w.commit().unwrap();
        r.reload();
        assert_eq!(paths(&r.search("zebras ", 5)), [r"C:\b.txt"]);
        w.finish().unwrap();
    }
}
