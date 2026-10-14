//! Compact in-RAM filename index plus the substring scan that queries it.
//!
//! Layout (struct-of-arrays, one allocation per field, no per-entry heap):
//!
//! * `lower`  – every name case-folded, each terminated by a NUL. One flat
//!   buffer, so a query is a single SIMD `memmem` pass over contiguous bytes.
//!   Names can't contain NUL and query terms never do, so a match can't
//!   straddle two entries.
//! * `orig`   – the same names with their original case, for display.
//! * `*_off`  – start offset of each name in the two buffers (plus a sentinel
//!   at the end so entry `i` is `off[i]..off[i+1]`).
//! * `parent` – index of the parent entry; paths are rebuilt on demand and
//!   never stored.

use memchr::memmem;
use rayon::prelude::*;
use rustc_hash::FxHashMap;

pub const NONE: u32 = u32::MAX;
const FLAG_DIR: u8 = 1;
const FLAG_ROOT: u8 = 2;

/// Below this many entries a single-threaded scan beats the fork/join cost.
const PAR_THRESHOLD: usize = 200_000;

/// Case-fold `name` and append it (without terminator) to `out`.
/// ASCII fast path; everything else goes through Unicode lowercase.
pub fn fold_into(name: &str, out: &mut Vec<u8>) {
    if name.is_ascii() {
        out.extend(name.bytes().map(|b| b.to_ascii_lowercase()));
    } else {
        let mut buf = [0u8; 4];
        for c in name.chars().flat_map(char::to_lowercase) {
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
}

#[derive(Default)]
pub struct Builder {
    lower: Vec<u8>,
    orig: Vec<u8>,
    lower_off: Vec<u32>,
    orig_off: Vec<u32>,
    parent: Vec<u32>,
    flags: Vec<u8>,
    // Scratch for the volume currently being loaded.
    vol_start: usize,
    ids: Vec<u64>,
    parent_ids: Vec<u64>,
    orphans: usize,
}

impl Builder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hint the expected entry/name sizes so big volumes don't realloc-copy.
    pub fn reserve(&mut self, entries: usize, name_bytes: usize) {
        self.lower.reserve(name_bytes);
        self.orig.reserve(name_bytes);
        for v in [&mut self.lower_off, &mut self.orig_off, &mut self.parent] {
            v.reserve(entries);
        }
        self.flags.reserve(entries);
        self.ids.reserve(entries);
        self.parent_ids.reserve(entries);
    }

    fn push_raw(&mut self, name: &str, flags: u8) {
        let lo = self.lower.len();
        let oo = self.orig.len();
        assert!(
            lo + name.len() * 3 + 2 < u32::MAX as usize && oo + name.len() + 1 < u32::MAX as usize,
            "name buffer exceeds 4 GiB"
        );
        self.lower_off.push(lo as u32);
        self.orig_off.push(oo as u32);
        fold_into(name, &mut self.lower);
        self.lower.push(0);
        self.orig.extend_from_slice(name.as_bytes());
        self.orig.push(0);
        self.parent.push(NONE);
        self.flags.push(flags);
    }

    /// Start a volume (or walked tree). `root_name` becomes the first path
    /// component (e.g. `C:`), `root_id` is the id children use as `parent`.
    pub fn begin_volume(&mut self, root_name: &str, root_id: u64) {
        self.vol_start = self.parent.len();
        self.ids.clear();
        self.parent_ids.clear();
        self.push_raw(root_name, FLAG_DIR | FLAG_ROOT);
        self.ids.push(root_id);
        self.parent_ids.push(u64::MAX);
    }

    /// Add one entry; `id`/`parent` only need to be consistent within the
    /// current volume.
    pub fn push(&mut self, id: u64, parent: u64, name: &str, is_dir: bool) {
        self.push_raw(name, if is_dir { FLAG_DIR } else { 0 });
        self.ids.push(id);
        self.parent_ids.push(parent);
    }

    /// Resolve parent ids to entry indices for the volume just loaded.
    /// Returns the number of orphans (entries whose parent was never seen).
    pub fn end_volume(&mut self) -> usize {
        let start = self.vol_start;
        let mut map: FxHashMap<u64, u32> =
            FxHashMap::with_capacity_and_hasher(self.ids.len(), Default::default());
        for (i, &id) in self.ids.iter().enumerate() {
            map.insert(id, (start + i) as u32);
        }
        let mut orphans = 0;
        for i in 1..self.ids.len() {
            let me = (start + i) as u32;
            let p = map.get(&self.parent_ids[i]).copied().unwrap_or(NONE);
            if p == NONE || p == me {
                orphans += 1;
                self.parent[start + i] = NONE;
            } else {
                self.parent[start + i] = p;
            }
        }
        self.orphans += orphans;
        self.ids = Vec::new();
        self.parent_ids = Vec::new();
        orphans
    }

    pub fn build(mut self) -> Index {
        self.lower_off.push(self.lower.len() as u32);
        self.orig_off.push(self.orig.len() as u32);
        self.lower.shrink_to_fit();
        self.orig.shrink_to_fit();
        self.lower_off.shrink_to_fit();
        self.orig_off.shrink_to_fit();
        self.parent.shrink_to_fit();
        self.flags.shrink_to_fit();
        Index {
            lower: self.lower,
            orig: self.orig,
            lower_off: self.lower_off,
            orig_off: self.orig_off,
            parent: self.parent,
            flags: self.flags,
            orphans: self.orphans,
        }
    }
}

pub struct Index {
    lower: Vec<u8>,
    orig: Vec<u8>,
    lower_off: Vec<u32>,
    orig_off: Vec<u32>,
    parent: Vec<u32>,
    flags: Vec<u8>,
    orphans: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Any,
    Files,
    Dirs,
}

#[derive(Clone, Copy, Debug)]
pub struct QueryOpts {
    /// Maximum hits returned (lowest entry indices first).
    pub limit: usize,
    /// Keep scanning past `limit` to report an exact total. Costs a full pass
    /// for common terms; leave off when only the first page is needed.
    pub count_total: bool,
    pub kind: Kind,
}

impl Default for QueryOpts {
    fn default() -> Self {
        QueryOpts {
            limit: 50,
            count_total: true,
            kind: Kind::Any,
        }
    }
}

#[derive(Debug)]
pub struct QueryResult {
    pub hits: Vec<u32>,
    /// Exact when `QueryOpts::count_total`; otherwise a lower bound.
    pub total: usize,
}

struct Plan {
    driver: Option<memmem::Finder<'static>>,
    extras: Vec<memmem::Finder<'static>>,
}

impl Index {
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    pub fn orphans(&self) -> usize {
        self.orphans
    }

    pub fn is_dir(&self, i: u32) -> bool {
        self.flags[i as usize] & FLAG_DIR != 0
    }

    pub fn name(&self, i: u32) -> &str {
        let i = i as usize;
        let s = &self.orig[self.orig_off[i] as usize..self.orig_off[i + 1] as usize - 1];
        std::str::from_utf8(s).unwrap_or("")
    }

    fn lower_name(&self, i: usize) -> &[u8] {
        &self.lower[self.lower_off[i] as usize..self.lower_off[i + 1] as usize - 1]
    }

    /// Bytes owned by the index (capacity, not just length).
    pub fn heap_bytes(&self) -> usize {
        self.lower.capacity()
            + self.orig.capacity()
            + 4 * (self.lower_off.capacity() + self.orig_off.capacity() + self.parent.capacity())
            + self.flags.capacity()
    }

    /// Rebuild the full path of entry `i` by walking parent links.
    pub fn path(&self, i: u32) -> String {
        let mut parts: Vec<&str> = Vec::with_capacity(16);
        let mut cur = i;
        let mut rooted = false;
        for _ in 0..4096 {
            parts.push(self.name(cur));
            if self.flags[cur as usize] & FLAG_ROOT != 0 {
                rooted = true;
                break;
            }
            match self.parent[cur as usize] {
                NONE => break,
                p => cur = p,
            }
        }
        if !rooted {
            parts.push("<orphan>");
        }
        parts.reverse();
        let mut out = parts.join("\\");
        if parts.len() == 1 && rooted && out.ends_with(':') {
            out.push('\\'); // "C:" -> "C:\"
        }
        out
    }

    fn plan(&self, query: &str) -> Plan {
        let mut terms: Vec<Vec<u8>> = query
            .split_whitespace()
            .map(|t| {
                let mut v = Vec::with_capacity(t.len());
                fold_into(t, &mut v);
                v.retain(|&b| b != 0);
                v
            })
            .filter(|t| !t.is_empty())
            .collect();
        if terms.is_empty() {
            return Plan {
                driver: None,
                extras: Vec::new(),
            };
        }
        // Longest term drives the scan: it is usually the rarest.
        let longest = (0..terms.len()).max_by_key(|&i| terms[i].len()).unwrap();
        let driver = terms.swap_remove(longest);
        Plan {
            driver: Some(memmem::Finder::new(&driver).into_owned()),
            extras: terms
                .iter()
                .map(|t| memmem::Finder::new(t).into_owned())
                .collect(),
        }
    }

    #[inline]
    fn accept(&self, e: usize, plan: &Plan, kind: Kind) -> bool {
        match kind {
            Kind::Any => {}
            Kind::Files if self.flags[e] & FLAG_DIR != 0 => return false,
            Kind::Dirs if self.flags[e] & FLAG_DIR == 0 => return false,
            _ => {}
        }
        if plan.extras.is_empty() {
            return true;
        }
        let name = self.lower_name(e);
        plan.extras.iter().all(|f| f.find(name).is_some())
    }

    fn scan_chunk(&self, e0: usize, e1: usize, plan: &Plan, opts: QueryOpts) -> (usize, Vec<u32>) {
        let mut hits = Vec::new();
        let mut count = 0usize;
        let push = |e: usize, count: &mut usize, hits: &mut Vec<u32>| -> bool {
            *count += 1;
            if hits.len() < opts.limit {
                hits.push(e as u32);
            }
            !opts.count_total && hits.len() >= opts.limit
        };

        let Some(driver) = &plan.driver else {
            for e in e0..e1 {
                if self.accept(e, plan, opts.kind) && push(e, &mut count, &mut hits) {
                    break;
                }
            }
            return (count, hits);
        };

        if !opts.count_total && opts.limit == 0 {
            return (0, hits);
        }
        let base = self.lower_off[e0] as usize;
        let hay = &self.lower[base..self.lower_off[e1] as usize];
        let mut pos = 0usize;
        let mut lo = e0;
        while pos < hay.len() {
            let Some(h) = driver.find(&hay[pos..]) else {
                break;
            };
            let abs = base + pos + h;
            // Entry containing `abs`: last start offset <= abs.
            let e = lo + self.lower_off[lo..e1].partition_point(|&o| o as usize <= abs) - 1;
            pos = self.lower_off[e + 1] as usize - base;
            lo = e + 1;
            if self.accept(e, plan, opts.kind) && push(e, &mut count, &mut hits) {
                break;
            }
        }
        (count, hits)
    }

    /// Case-insensitive substring search over file names. Whitespace-separated
    /// terms must all appear in the name (AND). Results are in index order.
    pub fn search(&self, query: &str, opts: QueryOpts) -> QueryResult {
        let plan = self.plan(query);
        let n = self.len();
        if n == 0 {
            return QueryResult {
                hits: Vec::new(),
                total: 0,
            };
        }
        let chunks = if n < PAR_THRESHOLD {
            1
        } else {
            rayon::current_num_threads() * 4
        };
        let ranges: Vec<(usize, usize)> = (0..chunks)
            .map(|k| (k * n / chunks, (k + 1) * n / chunks))
            .collect();
        let parts: Vec<(usize, Vec<u32>)> = if chunks == 1 {
            vec![self.scan_chunk(0, n, &plan, opts)]
        } else {
            ranges
                .par_iter()
                .map(|&(a, b)| self.scan_chunk(a, b, &plan, opts))
                .collect()
        };
        let mut total = 0;
        let mut hits = Vec::new();
        for (c, h) in parts {
            total += c;
            for e in h {
                if hits.len() < opts.limit {
                    hits.push(e);
                }
            }
        }
        QueryResult { hits, total }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Index {
        let mut b = Builder::new();
        b.begin_volume("C:", 5);
        b.push(10, 5, "Users", true);
        b.push(11, 10, "Zach", true);
        b.push(12, 11, "Résumé Final.PDF", false);
        b.push(13, 11, "notes.txt", false);
        b.push(14, 5, "Windows", true);
        b.push(15, 14, "notepad.exe", false);
        b.push(16, 999, "lost.txt", false); // parent never seen
        b.end_volume();
        b.build()
    }

    fn names(ix: &Index, r: &QueryResult) -> Vec<String> {
        r.hits.iter().map(|&i| ix.name(i).to_string()).collect()
    }

    #[test]
    fn case_insensitive_substring() {
        let ix = sample();
        let r = ix.search("NOTE", QueryOpts::default());
        assert_eq!(names(&ix, &r), ["notes.txt", "notepad.exe"]);
        assert_eq!(r.total, 2);
    }

    #[test]
    fn unicode_folding_and_original_case_display() {
        let ix = sample();
        let r = ix.search("résumé", QueryOpts::default());
        assert_eq!(names(&ix, &r), ["Résumé Final.PDF"]);
        let r = ix.search("RÉSUMÉ final", QueryOpts::default());
        assert_eq!(names(&ix, &r), ["Résumé Final.PDF"]);
    }

    #[test]
    fn multiple_terms_are_anded_and_order_free() {
        let ix = sample();
        assert_eq!(ix.search("txt note", QueryOpts::default()).total, 1);
        assert_eq!(ix.search("exe txt", QueryOpts::default()).total, 0);
    }

    #[test]
    fn repeated_match_in_one_name_counts_once() {
        let mut b = Builder::new();
        b.begin_volume("X:", 5);
        b.push(6, 5, "aaaa", false);
        b.end_volume();
        let ix = b.build();
        assert_eq!(ix.search("aa", QueryOpts::default()).total, 1);
    }

    #[test]
    fn kind_filter() {
        let ix = sample();
        let d = ix.search(
            "s",
            QueryOpts {
                kind: Kind::Dirs,
                ..Default::default()
            },
        );
        assert_eq!(names(&ix, &d), ["Users", "Windows"]);
        let f = ix.search(
            "s",
            QueryOpts {
                kind: Kind::Files,
                ..Default::default()
            },
        );
        assert!(names(&ix, &f).iter().all(|n| n.contains('.')));
    }

    #[test]
    fn limit_and_exact_total() {
        let ix = sample();
        let r = ix.search(
            "e",
            QueryOpts {
                limit: 2,
                ..Default::default()
            },
        );
        assert_eq!(r.hits.len(), 2);
        assert!(r.total > 2);
        let r2 = ix.search(
            "e",
            QueryOpts {
                limit: 2,
                count_total: false,
                ..Default::default()
            },
        );
        assert_eq!(r2.hits, r.hits);
    }

    #[test]
    fn empty_query_matches_everything() {
        let ix = sample();
        assert_eq!(ix.search("  ", QueryOpts::default()).total, ix.len());
    }

    #[test]
    fn paths_and_orphans() {
        let ix = sample();
        let id = ix.search("resume", QueryOpts::default()).hits;
        assert!(id.is_empty()); // accent-sensitive on purpose
        let id = ix.search("final", QueryOpts::default()).hits[0];
        assert_eq!(ix.path(id), "C:\\Users\\Zach\\Résumé Final.PDF");
        assert_eq!(ix.path(0), "C:\\");
        let lost = ix.search("lost", QueryOpts::default()).hits[0];
        assert_eq!(ix.path(lost), "<orphan>\\lost.txt");
        assert_eq!(ix.orphans(), 1);
    }

    #[test]
    fn parallel_scan_matches_serial_scan() {
        let mut b = Builder::new();
        b.begin_volume("T:", 5);
        let n = PAR_THRESHOLD * 2 + 17;
        for i in 0..n {
            let name = format!("file{}{}.dat", i, if i % 7 == 0 { "_seven" } else { "" });
            b.push(100 + i as u64, 5, &name, i % 50 == 0);
        }
        b.end_volume();
        let ix = b.build();
        let opts = QueryOpts {
            limit: 1_000_000,
            ..Default::default()
        };
        for q in ["seven", "file1", "9_s", "dat seven", "zzz"] {
            let par = ix.search(q, opts);
            let plan = ix.plan(q);
            let (count, hits) = ix.scan_chunk(0, ix.len(), &plan, opts);
            assert_eq!(par.total, count, "query {q}");
            assert_eq!(par.hits, hits, "query {q}");
        }
        // No-count mode returns the same first page.
        let full = ix.search(
            "file",
            QueryOpts {
                limit: 10,
                ..Default::default()
            },
        );
        let fast = ix.search(
            "file",
            QueryOpts {
                limit: 10,
                count_total: false,
                ..Default::default()
            },
        );
        assert_eq!(full.hits, fast.hits);
    }

    /// Differential test: the offset/partition machinery must agree with a
    /// naive per-name `contains` over randomized names (incl. non-ASCII).
    #[test]
    fn matches_naive_scan_on_random_names() {
        let alphabet: Vec<char> = "abAB.-_ 1é Éßİ日".chars().collect();
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut rnd = |n: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n as u64) as usize
        };
        let mut b = Builder::new();
        b.begin_volume("R:", 5);
        let mut names = Vec::new();
        for i in 0..5000 {
            let len = 1 + rnd(12);
            let name: String = (0..len).map(|_| alphabet[rnd(alphabet.len())]).collect();
            b.push(100 + i, 5, &name, rnd(5) == 0);
            names.push(name);
        }
        b.end_volume();
        let ix = b.build();
        let fold = |s: &str| {
            let mut v = Vec::new();
            fold_into(s, &mut v);
            String::from_utf8(v).unwrap()
        };
        for _ in 0..400 {
            let terms: Vec<String> = (0..1 + rnd(2))
                .map(|_| {
                    (0..1 + rnd(3))
                        .map(|_| alphabet[rnd(alphabet.len())])
                        .collect()
                })
                .filter(|t: &String| !t.trim().is_empty())
                .collect();
            let q = terms.join(" ");
            if q.trim().is_empty() {
                continue; // empty query also matches the root entry; covered elsewhere
            }
            let want: Vec<u32> = (0..names.len())
                .filter(|&i| {
                    let n = fold(&names[i]);
                    q.split_whitespace().all(|t| n.contains(&fold(t)))
                })
                .map(|i| i as u32 + 1) // +1: root entry is index 0
                .collect();
            let r = ix.search(
                &q,
                QueryOpts {
                    limit: usize::MAX,
                    ..Default::default()
                },
            );
            assert_eq!(r.hits, want, "query {q:?}");
            assert_eq!(r.total, want.len());
        }
    }

    #[test]
    fn two_volumes_keep_separate_roots() {
        let mut b = Builder::new();
        b.begin_volume("C:", 5);
        b.push(6, 5, "a.txt", false);
        b.end_volume();
        b.begin_volume("E:", 5);
        b.push(6, 5, "a.txt", false); // same ids, different volume
        b.end_volume();
        let ix = b.build();
        let hits = ix.search("a.txt", QueryOpts::default()).hits;
        assert_eq!(ix.path(hits[0]), "C:\\a.txt");
        assert_eq!(ix.path(hits[1]), "E:\\a.txt");
        assert_eq!(ix.orphans(), 0);
    }
}
