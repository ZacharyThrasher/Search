//! Streaming matcher for path terms.
//!
//! A path term is turned into a small DFA that is fed a path one byte at a
//! time. The index keeps one DFA state per *directory* (the state after
//! reading `C:\a\b\`), so testing an entry only means feeding its own name
//! from its parent's state, and most entries are decided without reading the
//! name at all (see `Verdict`). That replaces rebuilding every full path.

use regex_automata::dfa::{Automaton, dense};
use regex_automata::util::primitives::StateID;
use regex_automata::util::{start, syntax};
use regex_automata::{Anchored, MatchKind};

/// A DFA state plus "a match was already seen on the way here".
pub type Packed = u32;
const STICKY: u32 = 1 << 31;

/// What the state after a directory path says about an entry in it.
pub enum Verdict {
    /// The path already matched: every entry below matches.
    Match,
    /// No partial match is alive: no entry below can match.
    No,
    /// Depends on the entry's name.
    Feed,
}

pub struct PathMatcher {
    dfa: dense::DFA<Vec<u32>>,
    start: StateID,
    /// States that mean "nothing matched so far, nothing in progress".
    idle: [StateID; 3],
}

impl PathMatcher {
    /// `pattern` is a folded path pattern with `*` (any run) and `?` (one
    /// character); it matches anywhere in the path unless `anchor_start`, and
    /// has to reach the end of the path when `anchor_end`.
    pub fn new(pattern: &[u8], anchor_start: bool, anchor_end: bool) -> Option<PathMatcher> {
        let text = std::str::from_utf8(pattern).ok()?;
        let mut re = String::from("(?s)");
        for c in text.chars() {
            match c {
                '*' => re.push_str(".*"),
                '?' => re.push('.'),
                c => re.push_str(&regex_syntax::escape(&c.to_string())),
            }
        }
        if anchor_end {
            re.push('$');
        }
        let dfa = dense::Builder::new()
            .configure(
                dense::Config::new()
                    .start_kind(regex_automata::dfa::StartKind::Both)
                    .match_kind(MatchKind::All)
                    .minimize(false)
                    .accelerate(false)
                    .dfa_size_limit(Some(8 << 20))
                    .determinize_size_limit(Some(8 << 20)),
            )
            .syntax(syntax::Config::new().unicode(true).utf8(true))
            .build(&re)
            .ok()?;
        let anchored = if anchor_start {
            Anchored::Yes
        } else {
            Anchored::No
        };
        let start = dfa
            .start_state(&start::Config::new().anchored(anchored))
            .ok()?;
        // A control byte that is in no pattern moves the DFA to its resting state.
        let filler = (1u8..32).find(|b| !pattern.contains(b))?;
        let s1 = dfa.next_state(start, filler);
        let s2 = dfa.next_state(s1, filler);
        Some(PathMatcher {
            dfa,
            start,
            idle: [start, s1, s2],
        })
    }

    /// State before any byte of the path has been read.
    pub fn start(&self) -> Packed {
        self.start.as_u32()
    }

    /// Read `bytes` from `state`.
    pub fn feed(&self, state: Packed, bytes: &[u8]) -> Packed {
        let mut sticky = state & STICKY;
        let mut sid = StateID::new_unchecked((state & !STICKY) as usize);
        for &b in bytes {
            sid = self.dfa.next_state(sid, b);
            if self.dfa.is_match_state(sid) {
                sticky = STICKY;
            }
            if self.dfa.is_dead_state(sid) {
                break;
            }
        }
        sid.as_u32() | sticky
    }

    pub fn verdict(&self, state: Packed) -> Verdict {
        if state & STICKY != 0 {
            return Verdict::Match;
        }
        let sid = StateID::new_unchecked((state & !STICKY) as usize);
        if self.dfa.is_dead_state(sid) || self.idle.contains(&sid) {
            Verdict::No
        } else {
            Verdict::Feed
        }
    }

    /// Does the path that led to `state` match, taken to be complete?
    pub fn finish(&self, state: Packed) -> bool {
        if state & STICKY != 0 {
            return true;
        }
        let sid = StateID::new_unchecked((state & !STICKY) as usize);
        self.dfa.is_match_state(self.dfa.next_eoi_state(sid))
    }

    /// Test a whole path (used to check the matcher against the glob).
    pub fn is_match(&self, path: &[u8]) -> bool {
        self.finish(self.feed(self.start(), path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substring_and_globs() {
        let m = |pat: &str, start: bool, end: bool, path: &str| {
            PathMatcher::new(pat.as_bytes(), start, end)
                .unwrap()
                .is_match(path.as_bytes())
        };
        assert!(m("a\\b", false, false, "x\\a\\b\\c"));
        assert!(!m("a\\b", false, false, "x\\a\\c"));
        assert!(m("desk*\\*.png", false, true, "c:\\u\\desktop\\s\\a.png"));
        assert!(!m("desk*\\*.png", false, true, "c:\\u\\desktop\\a.gif"));
        assert!(m("c:\\u*", true, false, "c:\\users"));
        assert!(!m("c:\\u*", true, false, "d:\\c:\\users"));
        assert!(m("\u{e9}?\\", false, false, "x\\\u{e9}\u{e9}\\"));
        assert!(!m("\u{e9}?\\", false, false, "x\\\u{e9}\\"));
    }

    #[test]
    fn streaming_in_pieces_equals_one_go() {
        let m = PathMatcher::new(b"a\\b", false, false).unwrap();
        let d = m.feed(m.start(), b"c:\\a\\");
        assert!(matches!(m.verdict(d), Verdict::Feed));
        assert!(m.finish(m.feed(d, b"b.txt")));
        assert!(!m.finish(m.feed(d, b"c.txt")));
        let idle = m.feed(m.start(), b"c:\\x\\");
        assert!(matches!(m.verdict(idle), Verdict::No));
        let done = m.feed(m.feed(m.start(), b"a\\b\\"), b"z");
        assert!(matches!(m.verdict(done), Verdict::Match));
    }
}
