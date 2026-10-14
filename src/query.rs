//! The query language for file names (Everything-style, so it feels familiar).
//!
//! * `foo`        substring of the name, case-insensitive.
//! * `*` `?`      wildcards: any run of characters, exactly one character. A term
//!   with a wildcard must match the *whole* name: `*.png` is "ends in .png",
//!   `report*` is "starts with report", `*_reaction.gif` works as typed.
//! * space        AND between terms. `|` is OR (AND binds tighter, so
//!   `a b | c` is `(a b) | c`). A leading `!` negates a term.
//! * `ext:png;jpg` the name ends in one of the listed extensions.
//! * `\` or `/`   in a term: match against the full path instead of the name.
//!   Without wildcards it is a substring of the path; with wildcards it must
//!   end at the end of the path, and start at its beginning only when it begins
//!   with a drive (`c:\users\*\desktop\*.png`). `*` also crosses folders.
//! * `regex:...`  a regular expression on the name, case-insensitive. The rest
//!   of the word is the pattern (`|` is the regex's own there).
//! * `"two words"` quotes keep spaces (and `|`) literal inside one term.
//!
//! Half-typed input never errors: a broken regex (say a trailing `regex:(`)
//! just makes the query match nothing.
//!
//! Matching works on the same case-folded bytes the index stores, so a
//! literal fragment of a term can drive a `memmem` pass over the whole name
//! buffer (see `Group::drivers`) and only the few candidates are checked in full.

use std::ops::Range;

use memchr::memmem;
use regex::bytes::{Regex, RegexBuilder};

use crate::index::fold_into;

/// Terms beyond this many are ignored.
const MAX_TERMS: usize = 32;
/// Compiled-regex size caps: the helper is elevated and the pattern is typed
/// by whoever can reach the pipe.
const REGEX_SIZE_LIMIT: usize = 2 << 20;
const REGEX_DFA_LIMIT: usize = 8 << 20;

fn fold(text: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(text.len());
    fold_into(text, &mut v);
    v.retain(|&b| b != 0);
    v
}

fn char_len(lead: u8) -> usize {
    match lead {
        0xF0.. => 4,
        0xE0.. => 3,
        0xC0.. => 2,
        _ => 1,
    }
}

fn is_boundary(b: u8) -> bool {
    b & 0xC0 != 0x80
}

/// One run of characters between `*`s: literal bytes plus `?` (one character).
struct Seg {
    bytes: Vec<u8>,
    has_q: bool,
    /// Present when the segment has no `?`.
    finder: Option<memmem::Finder<'static>>,
}

impl Seg {
    fn new(bytes: &[u8]) -> Seg {
        let has_q = bytes.contains(&b'?');
        Seg {
            bytes: bytes.to_vec(),
            has_q,
            finder: (!has_q).then(|| memmem::Finder::new(bytes).into_owned()),
        }
    }

    /// End of the match when the segment matches `name` starting exactly at `at`.
    fn match_at(&self, name: &[u8], at: usize) -> Option<usize> {
        if !self.has_q {
            return name[at..]
                .starts_with(&self.bytes)
                .then(|| at + self.bytes.len());
        }
        let mut i = at;
        for &p in &self.bytes {
            let &b = name.get(i)?;
            if p == b'?' {
                i += char_len(b);
                if i > name.len() {
                    return None;
                }
            } else if b == p {
                i += 1;
            } else {
                return None;
            }
        }
        Some(i)
    }

    /// Leftmost match starting at or after `from`.
    fn find(&self, name: &[u8], from: usize) -> Option<(usize, usize)> {
        if let Some(f) = &self.finder {
            let i = f.find(&name[from..])?;
            return Some((from + i, from + i + self.bytes.len()));
        }
        (from..name.len())
            .filter(|&s| is_boundary(name[s]))
            .find_map(|s| self.match_at(name, s).map(|e| (s, e)))
    }

    /// Start of a match that ends exactly at the end of `name`, not before `from`.
    fn find_at_end(&self, name: &[u8], from: usize) -> Option<usize> {
        if !self.has_q {
            let s = name.len().checked_sub(self.bytes.len())?;
            return (s >= from && name.ends_with(&self.bytes)).then_some(s);
        }
        (from..name.len())
            .filter(|&s| is_boundary(name[s]))
            .find(|&s| self.match_at(name, s) == Some(name.len()))
    }
}

/// A folded pattern with `*` and `?`. Without any wildcard and without
/// anchors it is a plain substring.
pub struct Glob {
    segs: Vec<Seg>,
    anchor_start: bool,
    anchor_end: bool,
    wild: bool,
    shape: Shape,
}

/// The common single-literal patterns get a direct test.
#[derive(Clone, Copy)]
enum Shape {
    Contains,
    Prefix,
    Suffix,
    Exact,
    General,
}

impl Glob {
    fn new(pattern: &[u8], anchor_start: bool, anchor_end: bool) -> Glob {
        let segs: Vec<Seg> = pattern
            .split(|&b| b == b'*')
            .filter(|s| !s.is_empty())
            .map(Seg::new)
            .collect();
        let shape = match (&segs[..], anchor_start, anchor_end) {
            ([s], a, b) if s.finder.is_some() => match (a, b) {
                (false, false) => Shape::Contains,
                (true, false) => Shape::Prefix,
                (false, true) => Shape::Suffix,
                (true, true) => Shape::Exact,
            },
            _ => Shape::General,
        };
        Glob {
            segs,
            anchor_start,
            anchor_end,
            wild: pattern.iter().any(|&b| b == b'*' || b == b'?'),
            shape,
        }
    }

    /// A name term: a wildcard makes it match the whole name.
    fn name(pattern: &[u8]) -> Glob {
        let wild = pattern.iter().any(|&b| b == b'*' || b == b'?');
        Glob::new(
            pattern,
            wild && pattern[0] != b'*',
            wild && pattern[pattern.len() - 1] != b'*',
        )
    }

    /// A path term: end-anchored when it has wildcards, start-anchored only
    /// when it names a drive.
    fn path(pattern: &[u8]) -> Glob {
        let wild = pattern.iter().any(|&b| b == b'*' || b == b'?');
        let drive = pattern.len() >= 2 && pattern[0].is_ascii_alphabetic() && pattern[1] == b':';
        Glob::new(
            pattern,
            wild && drive,
            wild && pattern[pattern.len() - 1] != b'*',
        )
    }

    /// A pattern without wildcards: a literal substring.
    pub fn is_plain(&self) -> bool {
        !self.wild
    }

    #[inline]
    pub fn matches(&self, name: &[u8]) -> bool {
        match self.shape {
            Shape::Contains => self.segs[0]
                .finder
                .as_ref()
                .is_some_and(|f| f.find(name).is_some()),
            Shape::Prefix => name.starts_with(&self.segs[0].bytes),
            Shape::Suffix => name.ends_with(&self.segs[0].bytes),
            Shape::Exact => name == self.segs[0].bytes.as_slice(),
            Shape::General => self.run(name, None),
        }
    }

    fn run(&self, name: &[u8], mut out: Option<&mut Vec<Range<usize>>>) -> bool {
        let n = self.segs.len();
        let mut pos = 0usize;
        for (i, seg) in self.segs.iter().enumerate() {
            let first = i == 0 && self.anchor_start;
            let last = i + 1 == n && self.anchor_end;
            let (s, e) = if first && last {
                match seg.match_at(name, 0) {
                    Some(e) if e == name.len() => (0, e),
                    _ => return false,
                }
            } else if first {
                match seg.match_at(name, 0) {
                    Some(e) => (0, e),
                    None => return false,
                }
            } else if last {
                match seg.find_at_end(name, pos) {
                    Some(s) => (s, name.len()),
                    None => return false,
                }
            } else {
                match seg.find(name, pos) {
                    Some(r) => r,
                    None => return false,
                }
            };
            if let Some(o) = out.as_deref_mut() {
                o.push(s..e);
            }
            pos = e;
        }
        true
    }

    /// Where the literal parts matched, for highlighting. A plain term
    /// reports every occurrence; a wildcard term the one match.
    fn spans(&self, name: &[u8], out: &mut Vec<Range<usize>>) {
        if self.is_plain() && !self.anchor_start && !self.anchor_end {
            if let Some(seg) = self.segs.first() {
                let mut from = 0;
                while let Some((s, e)) = seg.find(name, from) {
                    out.push(s..e);
                    from = e;
                }
            }
            return;
        }
        let mark = out.len();
        if !self.run(name, Some(out)) {
            out.truncate(mark);
        }
    }

    /// The longest literal run, as bytes to `memmem` for in the flat name
    /// buffer. A run at the very end of an end-anchored pattern carries the
    /// NUL that terminates every name there, so `*.png` finds only names that
    /// end in `.png`.
    fn best_fragment(&self) -> Option<Vec<u8>> {
        let n = self.segs.len();
        let mut best: Option<Vec<u8>> = None;
        for (i, seg) in self.segs.iter().enumerate() {
            let tail = i + 1 == n && self.anchor_end;
            let runs: Vec<&[u8]> = seg.bytes.split(|&b| b == b'?').collect();
            for (k, run) in runs.iter().enumerate() {
                if run.is_empty() {
                    continue;
                }
                let mut v = run.to_vec();
                if tail && k + 1 == runs.len() {
                    v.push(0);
                }
                if best.as_ref().is_none_or(|b| v.len() > b.len()) {
                    best = Some(v);
                }
            }
        }
        best
    }

    /// `*.png`-shaped patterns (the last run is exactly a dot and an
    /// extension, nothing longer can narrow it further): the extension.
    fn ext_only(&self) -> Option<Vec<u8>> {
        if !self.anchor_end || self.segs.is_empty() {
            return None;
        }
        let last = self.segs.last()?;
        let e = last.bytes.strip_prefix(b".")?;
        (!e.is_empty() && !e.contains(&b'.') && !e.contains(&b'?')).then(|| e.to_vec())
    }

    /// For an end-anchored path pattern: the literal text after its last `\`,
    /// which can only sit in the file name, plus the terminating NUL.
    fn name_suffix_fragment(&self) -> Option<Vec<u8>> {
        if !self.anchor_end {
            return None;
        }
        let last = self.segs.last()?;
        let run = last.bytes.rsplit(|&b| b == b'?').next()?;
        let name_part = run.rsplit(|&b| b == b'\\').next()?;
        if name_part.is_empty() {
            return None;
        }
        let mut v = name_part.to_vec();
        v.push(0);
        Some(v)
    }
}

/// Literals every match of the regex must contain (folded, lowercase): the
/// regex's own prefix or suffix literals, whichever is longer. Taken from a
/// case-sensitive parse and lowered, so they are right for the case-insensitive
/// regex too (barring exotic equivalences such as U+017F, which is not
/// lowercase `s` in the folded name buffer). `None` when there is nothing
/// worth scanning for.
fn regex_literals(pattern: &str) -> Option<Vec<Vec<u8>>> {
    use regex_syntax::hir::literal::{ExtractKind, Extractor};
    let hir = regex_syntax::Parser::new().parse(pattern).ok()?;
    let mut best: Option<Vec<Vec<u8>>> = None;
    for kind in [ExtractKind::Prefix, ExtractKind::Suffix] {
        let mut ex = Extractor::new();
        ex.kind(kind).limit_class(8).limit_repeat(4).limit_total(32);
        let seq = ex.extract(&hir);
        let Some(lits) = seq.literals() else { continue };
        if lits.is_empty() || lits.len() > 16 {
            continue;
        }
        let folded: Vec<Vec<u8>> = lits
            .iter()
            .map(|l| {
                String::from_utf8_lossy(l.as_bytes())
                    .to_lowercase()
                    .into_bytes()
            })
            .collect();
        let min = folded.iter().map(Vec::len).min().unwrap_or(0);
        let better = best
            .as_ref()
            .is_none_or(|b| min > b.iter().map(Vec::len).min().unwrap_or(0));
        if min >= 2 && better {
            best = Some(folded);
        }
    }
    best
}

pub enum TermKind {
    /// Substring or whole-name wildcard match on the folded name.
    Name(Glob),
    /// Folded suffixes, each with its leading dot (`.png`).
    Ext(Vec<Vec<u8>>),
    /// Matched case-insensitively on the name as stored (original case).
    Regex(Regex, Option<Vec<Vec<u8>>>),
    /// Matched on the folded full path.
    Path(PathTerm),
}

/// A path term: the glob, plus what it takes to rebuild it as a streaming
/// matcher over directory paths.
pub struct PathTerm {
    pub glob: Glob,
    /// The folded pattern, `*` and `?` included.
    pub pattern: Vec<u8>,
    pub anchor_start: bool,
    pub anchor_end: bool,
    /// Position among the query's path terms (index into the per-search matchers).
    pub slot: usize,
}

/// Something to look for to find the candidates of a term.
pub(crate) enum DriverAlt {
    /// Bytes inside the flat folded name buffer.
    Bytes(Vec<u8>),
    /// Names whose extension (text after the last dot) is this.
    Ext(Vec<u8>),
}

impl DriverAlt {
    /// Rough selectivity: longer is rarer.
    fn weight(&self) -> usize {
        match self {
            DriverAlt::Bytes(b) => b.len(),
            DriverAlt::Ext(e) => e.len() + 2,
        }
    }
}

/// A resolved driver the index scans for.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Driver {
    Bytes(memmem::Finder<'static>),
    Ext(u16),
}

/// 16-bit hash of a folded extension; 0 is "no extension". Collisions are
/// fine: candidates are always verified.
pub fn ext_hash(ext: &[u8]) -> u16 {
    if ext.is_empty() {
        return 0;
    }
    let mut h: u32 = 0x811C_9DC5;
    for &b in ext {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    match (h ^ (h >> 16)) as u16 {
        0 => 1,
        v => v,
    }
}

/// The extension of a folded name: the text after its last dot.
pub fn name_ext(lower: &[u8]) -> &[u8] {
    match lower.iter().rposition(|&b| b == b'.') {
        Some(d) => &lower[d + 1..],
        None => &[],
    }
}

pub struct Term {
    pub neg: bool,
    pub kind: TermKind,
}

impl Term {
    /// Evaluation order: cheap name tests first, path building last.
    fn cost(&self) -> u8 {
        match self.kind {
            TermKind::Name(_) | TermKind::Ext(_) => 0,
            TermKind::Regex(..) => 1,
            TermKind::Path(_) => 2,
        }
    }

    /// Does the term, ignoring negation, match? A path term asks `ctx` for a
    /// quick verdict first and builds the full path only if it has none.
    fn hit(&self, lower: &[u8], orig: &[u8], ctx: &mut dyn EntryCtx) -> bool {
        match &self.kind {
            TermKind::Name(g) => g.matches(lower),
            TermKind::Ext(sfx) => sfx.iter().any(|s| lower.ends_with(s)),
            TermKind::Regex(r, _) => r.is_match(orig),
            TermKind::Path(p) => match ctx.path_verdict(p.slot, lower) {
                Some(v) => v,
                None => p.glob.matches(ctx.folded_path()),
            },
        }
    }

    /// Alternatives of which at least one must hold for this (positive) term
    /// to match, if it has any literal to offer.
    fn driver_alternatives(&self) -> Option<Vec<DriverAlt>> {
        match &self.kind {
            TermKind::Name(g) => g
                .ext_only()
                .map(|e| vec![DriverAlt::Ext(e)])
                .or_else(|| g.best_fragment().map(|f| vec![DriverAlt::Bytes(f)])),
            TermKind::Ext(sfx) => Some(
                sfx.iter()
                    .map(|s| {
                        // `.png` is a lookup in the extension column; `.tar.gz` is rarer as bytes.
                        if s[1..].contains(&b'.') {
                            let mut v = s.clone();
                            v.push(0);
                            DriverAlt::Bytes(v)
                        } else {
                            DriverAlt::Ext(s[1..].to_vec())
                        }
                    })
                    .collect(),
            ),
            TermKind::Path(p) => p
                .glob
                .name_suffix_fragment()
                .map(|f| vec![DriverAlt::Bytes(f)]),
            TermKind::Regex(_, lits) => lits
                .as_ref()
                .map(|l| l.iter().cloned().map(DriverAlt::Bytes).collect()),
        }
    }

    /// Whether finding the driver is the whole test, so it needs no recheck.
    fn exact_by_driver(&self, alts: &[DriverAlt]) -> bool {
        if alts.iter().any(|a| matches!(a, DriverAlt::Ext(_))) {
            return false; // the extension column is a hash: verify
        }
        match &self.kind {
            TermKind::Name(g) => g.is_plain(),
            TermKind::Ext(_) => true,
            _ => false,
        }
    }
}

/// What the scan knows about the entry being tested, beyond its name.
pub trait EntryCtx {
    /// Case-folded full path of the entry (built on first use).
    fn folded_path(&mut self) -> &[u8];
    /// Quick answer for path term `slot`, if the index can give one.
    fn path_verdict(&mut self, slot: usize, lower: &[u8]) -> Option<bool>;
}

/// Terms joined by AND.
pub struct Group {
    pub terms: Vec<Term>,
}

enum Ranking {
    /// Exact name, then prefix, then substring (today's order): `whole` is the
    /// plain terms joined by a space, `first` the first of them.
    Class { whole: Vec<u8>, first: Vec<u8> },
    /// Everything ties; shorter path, then name, decide.
    Flat,
}

/// A parsed query: OR of AND-groups.
pub struct Query {
    invalid: bool,
    pub(crate) groups: Vec<Group>,
    ranking: Ranking,
    /// Fragments to scan the name buffer for; every match of the query contains
    /// at least one. `None` when some group has nothing to offer (full scan).
    pub(crate) drivers: Option<Vec<Driver>>,
    /// Term of the single group whose test the driver already performs.
    pub(crate) skip: Option<usize>,
    /// Some term reads the name in its original case (a regex).
    pub(crate) needs_orig: bool,
    /// Path term every match has to satisfy (a positive one in a lone group):
    /// its directory verdict can reject an entry before anything else runs.
    pub(crate) prefilter: Option<usize>,
}

enum Tok {
    Or,
    Term(String),
}

/// `regex:` / `ext:` words keep `|` for themselves.
fn raw_word(text: &str) -> bool {
    let body = text.strip_prefix('!').unwrap_or(text);
    ["regex:", "ext:"].iter().any(|p| {
        body.get(..p.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(p))
    })
}

fn tokenize(text: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut had_quote = false;
    fn flush(cur: &mut String, had_quote: &mut bool, toks: &mut Vec<Tok>) {
        if !cur.is_empty() {
            toks.push(Tok::Term(std::mem::take(cur)));
        }
        *had_quote = false;
    }
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                had_quote = true;
            }
            c if c.is_whitespace() && !quoted => flush(&mut cur, &mut had_quote, &mut toks),
            '|' if !quoted && !raw_word(&cur) => {
                flush(&mut cur, &mut had_quote, &mut toks);
                toks.push(Tok::Or);
            }
            c => cur.push(c),
        }
    }
    flush(&mut cur, &mut had_quote, &mut toks);
    toks
}

/// A term built from one word; `Err` means the query can't match anything.
fn build_term(word: &str) -> Result<Option<Term>, ()> {
    let (neg, body) = match word.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, word),
    };
    let head = |p: &str| {
        body.get(..p.len())
            .filter(|h| h.eq_ignore_ascii_case(p))
            .map(|_| &body[p.len()..])
    };
    if let Some(pat) = head("regex:") {
        if pat.is_empty() {
            return Ok(None);
        }
        let re = RegexBuilder::new(pat)
            .case_insensitive(true)
            .size_limit(REGEX_SIZE_LIMIT)
            .dfa_size_limit(REGEX_DFA_LIMIT)
            .build()
            .map_err(|_| ())?;
        let lits = regex_literals(pat);
        return Ok(Some(Term {
            neg,
            kind: TermKind::Regex(re, lits),
        }));
    }
    if let Some(list) = head("ext:") {
        let sfx: Vec<Vec<u8>> = list
            .split([';', '|', ','])
            .map(|e| e.trim().trim_start_matches('.'))
            .filter(|e| !e.is_empty())
            .map(|e| {
                let mut v = vec![b'.'];
                v.extend(fold(e));
                v
            })
            .filter(|v| v.len() > 1)
            .collect();
        if sfx.is_empty() {
            return Ok(None);
        }
        return Ok(Some(Term {
            neg,
            kind: TermKind::Ext(sfx),
        }));
    }
    let is_path = body.contains(['\\', '/']);
    let pat = if is_path {
        fold(&body.replace('/', "\\"))
    } else {
        fold(body)
    };
    if pat.is_empty() {
        return Ok(None);
    }
    let kind = if is_path {
        let glob = Glob::path(&pat);
        TermKind::Path(PathTerm {
            anchor_start: glob.anchor_start,
            anchor_end: glob.anchor_end,
            glob,
            pattern: pat.clone(),
            slot: 0,
        })
    } else {
        TermKind::Name(Glob::name(&pat))
    };
    Ok(Some(Term { neg, kind }))
}

impl Query {
    pub fn parse(text: &str) -> Query {
        let mut raw_groups: Vec<Vec<Term>> = vec![Vec::new()];
        let mut invalid = false;
        let mut count = 0;
        for tok in tokenize(text) {
            match tok {
                Tok::Or => raw_groups.push(Vec::new()),
                Tok::Term(w) => {
                    if count >= MAX_TERMS {
                        break;
                    }
                    match build_term(&w) {
                        Ok(Some(t)) => {
                            count += 1;
                            raw_groups.last_mut().unwrap().push(t);
                        }
                        Ok(None) => {}
                        Err(()) => invalid = true,
                    }
                }
            }
        }
        let mut groups: Vec<Group> = raw_groups
            .into_iter()
            .filter(|g| !g.is_empty())
            .map(|mut terms| {
                terms.sort_by_key(Term::cost);
                Group { terms }
            })
            .collect();
        if invalid {
            groups.clear();
        }
        let mut slot = 0;
        for t in groups.iter_mut().flat_map(|g| g.terms.iter_mut()) {
            if let TermKind::Path(p) = &mut t.kind {
                p.slot = slot;
                slot += 1;
            }
        }
        let ranking = Query::ranking_for(&groups);
        let (drivers, skip) = Query::plan_drivers(&groups);
        let needs_orig = groups
            .iter()
            .flat_map(|g| &g.terms)
            .any(|t| matches!(t.kind, TermKind::Regex(..)));
        let prefilter = match groups.as_slice() {
            [g] => g.terms.iter().find_map(|t| match &t.kind {
                TermKind::Path(p) if !t.neg => Some(p.slot),
                _ => None,
            }),
            _ => None,
        };
        Query {
            invalid,
            groups,
            ranking,
            drivers,
            skip,
            needs_orig,
            prefilter,
        }
    }

    fn ranking_for(groups: &[Group]) -> Ranking {
        if let [g] = groups {
            let plain = |t: &Term| match &t.kind {
                TermKind::Name(gl) if !t.neg && gl.is_plain() => Some(gl.segs[0].bytes.clone()),
                _ => None,
            };
            let only_plain_or_filters = g
                .terms
                .iter()
                .all(|t| t.neg || plain(t).is_some() || matches!(t.kind, TermKind::Ext(_)));
            let plains: Vec<Vec<u8>> = g.terms.iter().filter_map(plain).collect();
            if only_plain_or_filters && !plains.is_empty() {
                return Ranking::Class {
                    whole: plains.join(&b' '),
                    first: plains[0].clone(),
                };
            }
        }
        Ranking::Flat
    }

    /// Best positive term of each group to scan for, if every group has one.
    fn plan_drivers(groups: &[Group]) -> (Option<Vec<Driver>>, Option<usize>) {
        let mut drivers = Vec::new();
        let mut skip = None;
        for g in groups {
            // Prefer the term whose weakest alternative is the rarest.
            let best = g
                .terms
                .iter()
                .enumerate()
                .filter(|(_, t)| !t.neg)
                .filter_map(|(i, t)| t.driver_alternatives().map(|a| (i, a)))
                .max_by_key(|(_, a)| {
                    (
                        a.iter().map(DriverAlt::weight).min().unwrap_or(0),
                        usize::MAX - a.len(),
                    )
                });
            let Some((i, alts)) = best else {
                return (None, None);
            };
            if groups.len() == 1 && g.terms[i].exact_by_driver(&alts) {
                skip = Some(i);
            }
            drivers.extend(alts.into_iter().map(|a| match a {
                DriverAlt::Bytes(b) => Driver::Bytes(memmem::Finder::new(&b).into_owned()),
                DriverAlt::Ext(e) => Driver::Ext(ext_hash(&e)),
            }));
        }
        (Some(drivers), skip)
    }

    /// The query's path terms, in `slot` order.
    pub fn path_terms(&self) -> Vec<&PathTerm> {
        let mut v: Vec<&PathTerm> = self
            .groups
            .iter()
            .flat_map(|g| &g.terms)
            .filter_map(|t| match &t.kind {
                TermKind::Path(p) => Some(p),
                _ => None,
            })
            .collect();
        v.sort_by_key(|p| p.slot);
        v
    }

    /// No usable term at all (blank, or only quotes and bangs).
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() && !self.invalid
    }

    /// A term that can't be compiled (half-typed regex); matches nothing.
    pub fn is_invalid(&self) -> bool {
        self.invalid
    }

    /// Results are ordered by path length then name only, no exact/prefix classes.
    pub fn is_flat(&self) -> bool {
        matches!(self.ranking, Ranking::Flat)
    }

    /// `(whole, first)` for exact/prefix/substring classes, when they apply.
    pub fn class_keys(&self) -> Option<(&[u8], &[u8])> {
        match &self.ranking {
            Ranking::Class { whole, first } => Some((whole, first)),
            Ranking::Flat => None,
        }
    }

    /// Does an entry with this folded name, original-case name and path match?
    ///
    /// `driven`: the caller only offers entries that contain a driver
    /// fragment, so the term `skip` names needs no test of its own.
    pub(crate) fn eval(
        &self,
        lower: &[u8],
        orig: &[u8],
        ctx: &mut dyn EntryCtx,
        driven: bool,
    ) -> bool {
        if self.invalid {
            return false;
        }
        self.groups.is_empty()
            || self.groups.iter().any(|g| {
                g.terms.iter().enumerate().all(|(i, t)| {
                    (driven && self.skip == Some(i)) || t.hit(lower, orig, ctx) != t.neg
                })
            })
    }

    /// Reference matcher on a full path (`C:\a\b.txt`); the last component is the name.
    pub fn matches_path(&self, full_path: &str) -> bool {
        struct P(Vec<u8>);
        impl EntryCtx for P {
            fn folded_path(&mut self) -> &[u8] {
                &self.0
            }
            fn path_verdict(&mut self, _: usize, _: &[u8]) -> Option<bool> {
                None
            }
        }
        let name = full_path.rsplit('\\').next().unwrap_or(full_path);
        self.eval(&fold(name), name.as_bytes(), &mut P(fold(full_path)), false)
    }

    /// Byte ranges of `name` to emphasise: what the positive terms matched.
    /// Path terms highlight nothing.
    pub fn highlight(&self, name: &str) -> Vec<Range<usize>> {
        let lname = fold(name);
        let folded_ok = lname.len() == name.len();
        let mut spans: Vec<Range<usize>> = Vec::new();
        for t in self.groups.iter().flat_map(|g| &g.terms).filter(|t| !t.neg) {
            match &t.kind {
                TermKind::Name(g) if folded_ok => g.spans(&lname, &mut spans),
                TermKind::Ext(sfx) if folded_ok => {
                    for s in sfx.iter().filter(|s| lname.ends_with(s)) {
                        spans.push(lname.len() - s.len() + 1..lname.len());
                    }
                }
                TermKind::Regex(r, _) => spans.extend(
                    r.find_iter(name.as_bytes())
                        .filter(|m| !m.is_empty())
                        .map(|m| m.range()),
                ),
                _ => {}
            }
        }
        spans.sort_by_key(|r| r.start);
        let mut merged: Vec<Range<usize>> = Vec::new();
        for s in spans {
            match merged.last_mut() {
                Some(last) if s.start <= last.end => last.end = last.end.max(s.end),
                _ => merged.push(s),
            }
        }
        // Folding can move characters while keeping the total length, so
        // offsets in the folded text may not be boundaries in `name`.
        if merged
            .iter()
            .any(|r| !name.is_char_boundary(r.start) || !name.is_char_boundary(r.end))
        {
            return Vec::new();
        }
        merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(q: &str, path: &str) -> bool {
        Query::parse(q).matches_path(path)
    }

    #[test]
    fn plain_terms_are_case_insensitive_substrings_anded() {
        assert!(m("note", r"C:\Notepad.exe"));
        assert!(m("pad NOTE", r"C:\Notepad.exe"));
        assert!(!m("note zzz", r"C:\Notepad.exe"));
        assert!(!m("zzz", r"C:\Notepad.exe"));
    }

    #[test]
    fn star_anchors_the_whole_name() {
        assert!(m("*.png", r"C:\a\cat.PNG"));
        assert!(!m("*.png", r"C:\a\cat.png.bak"));
        assert!(m("*_reaction.gif", r"C:\x\lol_reaction.gif"));
        assert!(!m("*_reaction.gif", r"C:\x\lol_reaction.gif2"));
        assert!(m("report*", r"C:\Report final.docx"));
        assert!(!m("report*", r"C:\my report.docx"));
        assert!(m("*report*", r"C:\my report.docx"));
        assert!(m("a*b*c", r"C:\xx\a1b2c"));
        assert!(!m("a*b*c", r"C:\xx\xa1b2c"));
        assert!(m("a*a", r"C:\aa"));
        assert!(!m("a*a", r"C:\a"));
        assert!(m("*", r"C:\anything"));
        assert!(m("**.txt", r"C:\a.txt"));
    }

    #[test]
    fn question_mark_is_one_character() {
        assert!(m("c?t", r"C:\cat"));
        assert!(!m("c?t", r"C:\caat"));
        assert!(!m("c?t", r"C:\ct"));
        assert!(m("?.txt", r"C:\a.txt"));
        assert!(!m("?.txt", r"C:\ab.txt"));
        assert!(m("h?llo*", r"C:\hällo wörld.txt")); // one character, not one byte
        assert!(m("*?.gif", r"C:\x.gif"));
        assert!(m("a?*c", r"C:\abxc"));
        assert!(!m("a?*c", r"C:\ac"));
        assert!(m("*a?c", r"C:\zzabc"));
        assert!(m("?", r"C:\é"));
    }

    #[test]
    fn or_not_and_precedence() {
        assert!(m("png | gif", r"C:\a.gif"));
        assert!(m("*.png|*.gif", r"C:\a.gif"));
        assert!(!m("*.png|*.gif", r"C:\a.jpg"));
        assert!(m("a b | c", r"C:\c"));
        assert!(m("a b | c", r"C:\ba"));
        assert!(!m("a b | c", r"C:\a"));
        assert!(m("report !draft", r"C:\report final"));
        assert!(!m("report !draft", r"C:\report draft"));
        assert!(m("!draft", r"C:\final"));
        assert!(!m("!*.tmp", r"C:\a.tmp"));
        assert!(m("a !", r"C:\a")); // a lone bang is nothing
        assert!(m("| a |", r"C:\a")); // empty groups vanish
    }

    #[test]
    fn extensions() {
        assert!(m("ext:png;jpg", r"C:\a.JPG"));
        assert!(m("ext:.png", r"C:\a.png"));
        assert!(!m("ext:png", r"C:\a.pngx"));
        assert!(!m("ext:png", r"C:\png"));
        assert!(m("ext:tar.gz", r"C:\a.tar.gz"));
        assert!(m("!ext:png", r"C:\a.gif"));
        assert!(m("ext:png|gif", r"C:\a.gif")); // `|` is a list separator here
        assert!(m("cat ext:png", r"C:\cat.png"));
        assert!(!m("dog ext:png", r"C:\cat.png"));
        assert!(Query::parse("ext:").is_empty());
    }

    #[test]
    fn path_terms() {
        assert!(m(r"users\zach", r"C:\Users\Zach\a.txt"));
        assert!(m("users/zach", r"C:\Users\Zach\a.txt"));
        assert!(!m(r"users\bob", r"C:\Users\Zach\a.txt"));
        assert!(m(r"desktop\*.png", r"C:\Users\Zach\Desktop\a.png"));
        assert!(m(r"desktop\*.png", r"C:\Users\Zach\Desktop\sub\a.png"));
        assert!(!m(r"desktop\*.png", r"C:\Users\Zach\Desktop\a.gif"));
        assert!(m(
            r"c:\users\*\desktop\*.png",
            r"C:\Users\Zach\Desktop\a.png"
        ));
        assert!(!m(
            r"c:\users\*\desktop\*.png",
            r"D:\C:\Users\Zach\Desktop\a.png"
        ));
        assert!(!m(r"d:\*.png", r"C:\d\a.png"));
        assert!(m(r"*\a?.txt", r"C:\x\ab.txt"));
        assert!(m(r"users\ ext:txt report", r"C:\Users\z\report.txt"));
    }

    #[test]
    fn regex_terms() {
        assert!(m(r"regex:^report\d+\.pdf$", r"C:\Report12.PDF"));
        assert!(!m(r"regex:^report\d+\.pdf$", r"C:\report.pdf"));
        assert!(m("regex:a|b", r"C:\xb")); // `|` stays inside the pattern
        assert!(m("regex:[A-Z]{3}", r"C:\abc")); // classes work despite case folding
        assert!(m("!regex:^a", r"C:\b"));
        assert!(m(r#"regex:"a b""#, r"C:\a b"));
        assert!(Query::parse("regex:").is_empty());
    }

    #[test]
    fn half_typed_input_matches_nothing_without_erroring() {
        for q in [
            "regex:(",
            "regex:[a-",
            "report regex:(",
            "regex:*",
            "a | regex:(\\",
        ] {
            let p = Query::parse(q);
            assert!(p.is_invalid(), "{q}");
            assert!(!p.matches_path(r"C:\anything"), "{q}");
            assert!(p.highlight("anything").is_empty());
        }
        assert!(!Query::parse("regex:(").is_empty());
        assert!(Query::parse("  ").is_empty());
        // Unterminated quote: the rest is one term.
        assert!(m("\"my fi", r"C:\my file.txt"));
    }

    #[test]
    fn quotes_keep_spaces_and_bars_literal() {
        assert!(m("\"my file\"", r"C:\my file.txt"));
        assert!(!m("\"my file\"", r"C:\file my.txt"));
        assert!(m("\"my *.txt\"", r"C:\my file.txt"));
        assert!(m("\"a|b\"", r"C:\xa|b"));
        assert!(!m("\"a|b\"", r"C:\a"));
    }

    #[test]
    fn huge_regex_is_refused() {
        assert!(Query::parse(r"regex:(\w{200}){200}").is_invalid());
    }

    #[test]
    fn ranking_modes() {
        assert_eq!(
            Query::parse("a b").class_keys(),
            Some((&b"a b"[..], &b"a"[..]))
        );
        assert!(Query::parse("a !b ext:txt").class_keys().is_some());
        assert!(Query::parse("*.png").is_flat());
        assert!(Query::parse("a | b").is_flat());
        assert!(Query::parse("regex:a").is_flat());
        assert!(Query::parse(r"a\b").is_flat());
        assert!(Query::parse("ext:png").is_flat());
    }

    #[test]
    fn drivers_are_chosen_when_there_is_a_literal() {
        assert_eq!(
            Query::parse("*.png").drivers.as_ref().map(Vec::len),
            Some(1)
        );
        assert_eq!(
            Query::parse("ext:png;jpg").drivers.as_ref().map(Vec::len),
            Some(2)
        );
        assert_eq!(
            Query::parse("a | b").drivers.as_ref().map(Vec::len),
            Some(2)
        );
        assert!(Query::parse("???").drivers.is_none());
        assert!(Query::parse("regex:a").drivers.is_none()); // too short to be worth a scan
        assert!(Query::parse("a | regex:b").drivers.is_none());
        assert_eq!(
            Query::parse(r"regex:^Report\d+$")
                .drivers
                .as_ref()
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(
            Query::parse(r"regex:\d{5}\.png$")
                .drivers
                .as_ref()
                .map(Vec::len),
            Some(1)
        );
        assert!(Query::parse(r"regex:(cat|dog)s?").drivers.is_some());
        assert!(Query::parse(r"regex:.*").drivers.is_none());
        assert!(Query::parse("!a").drivers.is_none());
        assert_eq!(
            Query::parse("regex:x report")
                .drivers
                .as_ref()
                .map(Vec::len),
            Some(1)
        );
        // A path pattern ending in a literal can still be driven by the name.
        assert_eq!(
            Query::parse(r"src\*.rs").drivers.as_ref().map(Vec::len),
            Some(1)
        );
        assert!(Query::parse(r"src\main").drivers.is_none());
    }

    #[test]
    fn highlighting() {
        let h = |q: &str, n: &str| Query::parse(q).highlight(n);
        assert_eq!(h("note", "Notepad++.exe"), vec![0..4]);
        assert_eq!(h("pad NOTE", "Notepad++.exe"), vec![0..7]);
        assert_eq!(h("a", "a-a-a"), vec![0..1, 2..3, 4..5]);
        assert_eq!(h("*.png", "cat.png"), vec![3..7]);
        assert_eq!(h("c*t*.png", "cart.png"), vec![0..1, 3..8]);
        assert_eq!(h("ext:png", "cat.png"), vec![4..7]);
        assert_eq!(h(r"regex:\d+", "a12b3"), vec![1..3, 4..5]);
        assert_eq!(h("!a", "abc"), Vec::<Range<usize>>::new());
        assert_eq!(h("c?t", "cat"), vec![0..3]);
        assert!(h("stan", "İstanbul").is_empty());
    }

    /// A random glob checked against the same pattern turned into a regex.
    #[test]
    fn globs_agree_with_a_regex_oracle() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = |n: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n as u64) as usize
        };
        let name_chars: Vec<char> = "abAB.\u{e9}\u{c9}_".chars().collect();
        let pat_chars: Vec<char> = "abb.*?*\u{e9}_".chars().collect();
        for _ in 0..3000 {
            let pat: String = (0..1 + rnd(6))
                .map(|_| pat_chars[rnd(pat_chars.len())])
                .collect();
            let wild = pat.contains(['*', '?']);
            let mut re = String::from("(?s)");
            if wild && !pat.starts_with('*') {
                re.push('^');
            }
            for c in pat.chars() {
                match c {
                    '*' => re.push_str(".*"),
                    '?' => re.push('.'),
                    c => re.push_str(&regex::escape(&c.to_string())),
                }
            }
            if wild && !pat.ends_with('*') {
                re.push('$');
            }
            let oracle = regex::Regex::new(&re).unwrap();
            let q = Query::parse(&pat);
            for _ in 0..20 {
                let name: String = (0..rnd(8))
                    .map(|_| name_chars[rnd(name_chars.len())])
                    .collect();
                if name.is_empty() {
                    continue;
                }
                let want = oracle.is_match(&name.to_lowercase());
                let got = q.matches_path(&format!("C:\\{name}"));
                assert_eq!(got, want, "pattern {pat:?} name {name:?}");
            }
        }
    }
}
