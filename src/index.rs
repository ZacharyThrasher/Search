//! Compact in-RAM filename index plus the substring scan that queries it.
//!
//! One `Index` holds one volume (or walked tree). Layout is struct-of-arrays,
//! one allocation per field, no per-entry heap:
//!
//! * `lower`  – every name case-folded, each terminated by a NUL. One flat
//!   buffer, so a query is a single SIMD `memmem` pass over contiguous bytes.
//!   Names can't contain NUL and query terms never do, so a match can't
//!   straddle two entries.
//! * `orig`   – the same names with their original case, for display.
//! * `*_off`  – start offset of each name in the two buffers (plus a sentinel
//!   at the end so entry `i` is `off[i]..off[i+1]`).
//! * `rec`    – the entry's own record number (NTFS MFT record, or a walk id).
//! * `parent` – the *record number* of the parent. Paths are rebuilt on demand
//!   by hopping through `by_rec`, so renaming or moving a directory never has
//!   to touch its children.
//! * `by_rec` – dense record number -> entry index table (`NONE` when absent).
//!
//! The index can be edited in place for live updates: new names are appended,
//! removed names are tombstoned (their folded bytes become `0xFF`, which no
//! UTF-8 query term can contain) and `compacted` squeezes the dead entries out.

use memchr::memmem;
use rayon::prelude::*;

pub const NONE: u32 = u32::MAX;
const FLAG_DIR: u8 = 1;
const FLAG_ROOT: u8 = 2;
const FLAG_DEAD: u8 = 4;

/// Largest record number the lookup table accepts (a 1 GiB table). Real MFTs
/// stay far below this; anything above is treated as garbage and dropped.
pub const MAX_REC: u64 = 1 << 28;

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

fn narrow(id: u64) -> Option<u32> {
    (id < MAX_REC).then_some(id as u32)
}

/// What `Index::upsert` did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Applied {
    Added,
    Renamed,
    Moved,
    Unchanged,
    /// Root record, empty name, out-of-range id, or name buffers full.
    Skipped,
}

/// Borrowed view of every column, in snapshot order.
pub struct Parts<'a> {
    pub lower: &'a [u8],
    pub orig: &'a [u8],
    pub lower_off: &'a [u32],
    pub orig_off: &'a [u32],
    pub parent: &'a [u32],
    pub rec: &'a [u32],
    pub flags: &'a [u8],
    pub orphans: usize,
}

/// Owned columns, as read back from a snapshot.
pub struct OwnedParts {
    pub lower: Vec<u8>,
    pub orig: Vec<u8>,
    pub lower_off: Vec<u32>,
    pub orig_off: Vec<u32>,
    pub parent: Vec<u32>,
    pub rec: Vec<u32>,
    pub flags: Vec<u8>,
    pub orphans: usize,
}

pub struct Index {
    lower: Vec<u8>,
    orig: Vec<u8>,
    lower_off: Vec<u32>,
    orig_off: Vec<u32>,
    parent: Vec<u32>,
    rec: Vec<u32>,
    flags: Vec<u8>,
    by_rec: Vec<u32>,
    orphans: usize,
    dead: usize,
    /// Set once the index is built: later appends grow the columns by a few
    /// percent instead of doubling, so a live index never holds (or copies)
    /// twice the memory it needs.
    live_growth: bool,
}

/// Spare room kept in every column of a finished index (2%, at least 4096).
pub fn spare(len: usize) -> usize {
    len / 50 + 4096
}

/// Make room for `need` more items without the doubling `Vec` would do.
fn make_room<T>(v: &mut Vec<T>, need: usize, live: bool) {
    if live && v.capacity() - v.len() < need {
        v.reserve_exact(need + spare(v.len()));
    }
}

/// Collects entries for one volume, then resolves parents and hands back an
/// `Index`. Call `begin_volume` once, `push` per entry, then `build`.
pub struct Builder {
    ix: Index,
    begun: bool,
    resolved: bool,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    pub fn new() -> Self {
        Builder {
            ix: Index::empty(),
            begun: false,
            resolved: false,
        }
    }

    /// Hint the expected entry/name sizes so big volumes don't realloc-copy.
    pub fn reserve(&mut self, entries: usize, name_bytes: usize) {
        let ix = &mut self.ix;
        ix.lower.reserve(name_bytes);
        ix.orig.reserve(name_bytes);
        for v in [&mut ix.lower_off, &mut ix.orig_off, &mut ix.parent, &mut ix.rec] {
            v.reserve(entries);
        }
        ix.flags.reserve(entries);
    }

    /// Start the volume (or walked tree). `root_name` becomes the first path
    /// component (e.g. `C:`), `root_id` is the id children use as `parent`.
    pub fn begin_volume(&mut self, root_name: &str, root_id: u64) {
        assert!(!self.begun, "an Index holds exactly one volume");
        self.begun = true;
        let rec = narrow(root_id).expect("root id out of range");
        self.ix
            .append(rec, NONE, root_name, FLAG_DIR | FLAG_ROOT)
            .expect("root name too large");
    }

    /// Add one entry; `id`/`parent` only need to be consistent within the
    /// volume. Ids outside `0..MAX_REC` are dropped (and counted as orphans).
    pub fn push(&mut self, id: u64, parent: u64, name: &str, is_dir: bool) {
        let Some(rec) = narrow(id) else {
            self.ix.orphans += 1;
            return;
        };
        let parent = narrow(parent).filter(|&p| p != rec).unwrap_or(NONE);
        self.ix
            .append(rec, parent, name, if is_dir { FLAG_DIR } else { 0 })
            .expect("name buffer exceeds 4 GiB");
    }

    /// Build the record lookup table and count entries whose parent was never
    /// seen. Returns the number of orphans. Idempotent.
    pub fn end_volume(&mut self) -> usize {
        if !self.resolved {
            self.resolved = true;
            self.ix.rebuild_lookup();
            self.ix.count_orphans();
        }
        self.ix.orphans
    }

    pub fn build(mut self) -> Index {
        self.end_volume();
        let ix = &mut self.ix;
        ix.lower.shrink_to(ix.lower.len() + spare(ix.lower.len()));
        ix.orig.shrink_to(ix.orig.len() + spare(ix.orig.len()));
        ix.lower_off.shrink_to(ix.lower_off.len() + spare(ix.lower_off.len()));
        ix.orig_off.shrink_to(ix.orig_off.len() + spare(ix.orig_off.len()));
        ix.parent.shrink_to(ix.parent.len() + spare(ix.parent.len()));
        ix.rec.shrink_to(ix.rec.len() + spare(ix.rec.len()));
        ix.flags.shrink_to(ix.flags.len() + spare(ix.flags.len()));
        ix.live_growth = true;
        self.ix
    }
}

impl Index {
    fn empty() -> Index {
        Index {
            lower: Vec::new(),
            orig: Vec::new(),
            lower_off: vec![0],
            orig_off: vec![0],
            parent: Vec::new(),
            rec: Vec::new(),
            flags: Vec::new(),
            by_rec: Vec::new(),
            orphans: 0,
            dead: 0,
            live_growth: false,
        }
    }

    /// Append one entry to the columns. Does not touch `by_rec`.
    /// `None` when the name buffers would pass 4 GiB.
    fn append(&mut self, rec: u32, parent: u32, name: &str, flags: u8) -> Option<u32> {
        let lo = self.lower.len();
        let oo = self.orig.len();
        if lo + name.len() * 3 + 2 >= u32::MAX as usize || oo + name.len() + 1 >= u32::MAX as usize
        {
            return None;
        }
        let live = self.live_growth;
        make_room(&mut self.lower, name.len() * 3 + 2, live);
        make_room(&mut self.orig, name.len() + 1, live);
        make_room(&mut self.lower_off, 1, live);
        make_room(&mut self.orig_off, 1, live);
        make_room(&mut self.parent, 1, live);
        make_room(&mut self.rec, 1, live);
        make_room(&mut self.flags, 1, live);
        fold_into(name, &mut self.lower);
        self.lower.push(0);
        self.orig.extend_from_slice(name.as_bytes());
        self.orig.push(0);
        self.lower_off.push(self.lower.len() as u32);
        self.orig_off.push(self.orig.len() as u32);
        self.parent.push(parent);
        self.rec.push(rec);
        self.flags.push(flags);
        Some((self.rec.len() - 1) as u32)
    }

    /// Recompute `by_rec` from `rec`. A record number seen twice keeps its
    /// last entry; earlier ones are retired.
    fn rebuild_lookup(&mut self) {
        let max = self.rec.iter().copied().max().map_or(0, |m| m as usize + 1);
        self.by_rec.clear();
        self.by_rec.reserve_exact(max + spare(max));
        self.by_rec.resize(max, NONE);
        for i in 0..self.rec.len() {
            if self.flags[i] & FLAG_DEAD != 0 {
                continue;
            }
            let slot = self.by_rec[self.rec[i] as usize];
            if slot != NONE {
                self.retire(slot as usize);
            }
            self.by_rec[self.rec[i] as usize] = i as u32;
        }
    }

    fn count_orphans(&mut self) {
        let mut orphans = 0;
        for i in 0..self.rec.len() {
            if self.flags[i] & (FLAG_ROOT | FLAG_DEAD) != 0 {
                continue;
            }
            if self.parent[i] == NONE || self.lookup(self.parent[i]).is_none() {
                orphans += 1;
            }
        }
        self.orphans += orphans;
    }

    /// Mark an entry dead. Its folded bytes become 0xFF so scans skip it.
    fn retire(&mut self, i: usize) {
        if self.flags[i] & FLAG_DEAD != 0 {
            return;
        }
        self.flags[i] |= FLAG_DEAD;
        self.dead += 1;
        let (a, b) = (self.lower_off[i] as usize, self.lower_off[i + 1] as usize - 1);
        self.lower[a..b].fill(0xFF);
    }

    fn lookup(&self, rec: u32) -> Option<u32> {
        self.by_rec.get(rec as usize).copied().filter(|&i| i != NONE)
    }

    /// Entry currently holding record number `rec`, if any.
    pub fn find_record(&self, rec: u64) -> Option<u32> {
        self.lookup(narrow(rec)?)
    }

    fn orig_bytes(&self, i: usize) -> &[u8] {
        &self.orig[self.orig_off[i] as usize..self.orig_off[i + 1] as usize - 1]
    }

    /// Create or update the entry for `rec`. A changed name retires the old
    /// slot and appends a new one; a pure move or flag change edits in place.
    pub fn upsert(&mut self, rec: u64, parent: u64, name: &str, is_dir: bool) -> Applied {
        let Some(rec) = narrow(rec) else {
            return Applied::Skipped;
        };
        if name.is_empty() || self.rec.first() == Some(&rec) {
            return Applied::Skipped; // the root never changes
        }
        let parent = narrow(parent).filter(|&p| p != rec).unwrap_or(NONE);
        let dir_flag = if is_dir { FLAG_DIR } else { 0 };
        let mut replaced = false;
        if let Some(i) = self.lookup(rec) {
            let i = i as usize;
            if self.orig_bytes(i) == name.as_bytes() {
                let mut changed = false;
                if self.parent[i] != parent {
                    self.parent[i] = parent;
                    changed = true;
                }
                if self.flags[i] & FLAG_DIR != dir_flag {
                    self.flags[i] = self.flags[i] & !FLAG_DIR | dir_flag;
                    changed = true;
                }
                return if changed {
                    Applied::Moved
                } else {
                    Applied::Unchanged
                };
            }
            // Case-only rename of the same byte length: rewrite the display
            // name where it sits.
            let mut folded = Vec::with_capacity(name.len());
            fold_into(name, &mut folded);
            if folded == self.lower_name(i) && name.len() == self.orig_bytes(i).len() {
                let a = self.orig_off[i] as usize;
                self.orig[a..a + name.len()].copy_from_slice(name.as_bytes());
                self.parent[i] = parent;
                return Applied::Renamed;
            }
            self.retire(i);
            self.by_rec[rec as usize] = NONE;
            replaced = true;
        }
        let Some(i) = self.append(rec, parent, name, dir_flag) else {
            return Applied::Skipped;
        };
        if rec as usize >= self.by_rec.len() {
            let need = rec as usize + 1 - self.by_rec.len();
            make_room(&mut self.by_rec, need, self.live_growth);
            self.by_rec.resize(rec as usize + 1, NONE);
        }
        self.by_rec[rec as usize] = i;
        if replaced {
            Applied::Renamed
        } else {
            Applied::Added
        }
    }

    /// Drop the entry for `rec`. Returns whether there was one.
    pub fn remove(&mut self, rec: u64) -> bool {
        let Some(rec) = narrow(rec) else {
            return false;
        };
        if self.rec.first() == Some(&rec) {
            return false;
        }
        let Some(i) = self.lookup(rec) else {
            return false;
        };
        self.retire(i as usize);
        self.by_rec[rec as usize] = NONE;
        true
    }

    /// Entries removed or renamed away since the last compaction.
    pub fn dead_len(&self) -> usize {
        self.dead
    }

    /// Worth squeezing: more than 5% of the entries (and a floor, so small
    /// indexes don't churn) are dead.
    pub fn needs_compaction(&self) -> bool {
        self.dead >= 50_000 && self.dead * 20 > self.len()
    }

    /// A copy without the dead entries, in the same order.
    pub fn compacted(&self) -> Index {
        let live = self.len() - self.dead;
        let mut out = Index::empty();
        out.lower.reserve(self.lower.len());
        out.orig.reserve(self.orig.len());
        for v in [
            &mut out.lower_off,
            &mut out.orig_off,
            &mut out.parent,
            &mut out.rec,
        ] {
            v.reserve(live);
        }
        out.flags.reserve(live);
        for i in 0..self.len() {
            if self.flags[i] & FLAG_DEAD != 0 {
                continue;
            }
            out.lower.extend_from_slice(
                &self.lower[self.lower_off[i] as usize..self.lower_off[i + 1] as usize],
            );
            out.orig.extend_from_slice(
                &self.orig[self.orig_off[i] as usize..self.orig_off[i + 1] as usize],
            );
            out.lower_off.push(out.lower.len() as u32);
            out.orig_off.push(out.orig.len() as u32);
            out.parent.push(self.parent[i]);
            out.rec.push(self.rec[i]);
            out.flags.push(self.flags[i]);
        }
        out.by_rec = vec![NONE; self.by_rec.len()];
        for (i, &r) in out.rec.iter().enumerate() {
            out.by_rec[r as usize] = i as u32;
        }
        out.orphans = self.orphans;
        out.live_growth = true;
        out.lower.shrink_to(out.lower.len() + spare(out.lower.len()));
        out.orig.shrink_to(out.orig.len() + spare(out.orig.len()));
        out
    }

    pub fn parts(&self) -> Parts<'_> {
        Parts {
            lower: &self.lower,
            orig: &self.orig,
            lower_off: &self.lower_off,
            orig_off: &self.orig_off,
            parent: &self.parent,
            rec: &self.rec,
            flags: &self.flags,
            orphans: self.orphans,
        }
    }

    /// Rebuild an index from snapshot columns, checking everything a corrupt
    /// file could break (offsets, terminators, ranges, duplicate records).
    pub fn from_parts(p: OwnedParts) -> Result<Index, String> {
        let n = p.parent.len();
        if p.rec.len() != n
            || p.flags.len() != n
            || p.lower_off.len() != n + 1
            || p.orig_off.len() != n + 1
        {
            return Err("column lengths disagree".into());
        }
        let check = |off: &[u32], buf: &[u8], what: &str| -> Result<(), String> {
            if off[0] != 0 || off[n] as usize != buf.len() {
                return Err(format!("{what} offsets do not span the buffer"));
            }
            // Strictly increasing (each name is at least its NUL) and NUL-terminated.
            let bad = off
                .par_windows(2)
                .any(|w| w[1] <= w[0] || buf.get(w[1] as usize - 1) != Some(&0));
            if bad {
                Err(format!("{what} offsets or terminators are corrupt"))
            } else {
                Ok(())
            }
        };
        check(&p.lower_off, &p.lower, "lower")?;
        check(&p.orig_off, &p.orig, "orig")?;
        if p.rec.par_iter().any(|&r| r as u64 >= MAX_REC) {
            return Err("record number out of range".into());
        }
        let dead = p.flags.iter().filter(|&&f| f & FLAG_DEAD != 0).count();
        let mut ix = Index {
            lower: p.lower,
            orig: p.orig,
            lower_off: p.lower_off,
            orig_off: p.orig_off,
            parent: p.parent,
            rec: p.rec,
            flags: p.flags,
            by_rec: Vec::new(),
            orphans: p.orphans,
            dead,
            live_growth: true,
        };
        ix.rebuild_lookup();
        if ix.dead != dead {
            return Err("duplicate record numbers".into());
        }
        if n > 0 && ix.flags[0] & FLAG_ROOT == 0 {
            return Err("first entry is not the root".into());
        }
        Ok(ix)
    }
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
    /// Entry slots, including dead ones (see `live_len`).
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn live_len(&self) -> usize {
        self.parent.len() - self.dead
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// Entries whose parent was unknown when the index was built or loaded.
    pub fn orphans(&self) -> usize {
        self.orphans
    }

    pub fn is_dir(&self, i: u32) -> bool {
        self.flags[i as usize] & FLAG_DIR != 0
    }

    pub fn name(&self, i: u32) -> &str {
        std::str::from_utf8(self.orig_bytes(i as usize)).unwrap_or("")
    }

    fn lower_name(&self, i: usize) -> &[u8] {
        &self.lower[self.lower_off[i] as usize..self.lower_off[i + 1] as usize - 1]
    }

    /// Bytes owned by the index (capacity, not just length).
    pub fn heap_bytes(&self) -> usize {
        self.lower.capacity()
            + self.orig.capacity()
            + 4 * (self.lower_off.capacity()
                + self.orig_off.capacity()
                + self.parent.capacity()
                + self.rec.capacity()
                + self.by_rec.capacity())
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
            let p = self.parent[cur as usize];
            match self.lookup(p) {
                Some(next) if p != NONE && next != cur => cur = next,
                _ => break,
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
        let flags = self.flags[e];
        if flags & FLAG_DEAD != 0 {
            return false;
        }
        match kind {
            Kind::Any => {}
            Kind::Files if flags & FLAG_DIR != 0 => return false,
            Kind::Dirs if flags & FLAG_DIR == 0 => return false,
            _ => {}
        }
        if plan.extras.is_empty() {
            return true;
        }
        let name = self.lower_name(e);
        plan.extras.iter().all(|f| f.find(name).is_some())
    }

    /// Call `f(entry)` for every accepted entry in `e0..e1`, in index order,
    /// until it returns `true` (stop).
    fn for_each_match(
        &self,
        e0: usize,
        e1: usize,
        plan: &Plan,
        kind: Kind,
        f: &mut dyn FnMut(usize) -> bool,
    ) {
        let Some(driver) = &plan.driver else {
            for e in e0..e1 {
                if self.accept(e, plan, kind) && f(e) {
                    return;
                }
            }
            return;
        };
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
            if self.accept(e, plan, kind) && f(e) {
                return;
            }
        }
    }

    fn scan_chunk(&self, e0: usize, e1: usize, plan: &Plan, opts: QueryOpts) -> (usize, Vec<u32>) {
        let mut hits = Vec::new();
        let mut count = 0usize;
        if plan.driver.is_some() && !opts.count_total && opts.limit == 0 {
            return (0, hits);
        }
        self.for_each_match(e0, e1, plan, opts.kind, &mut |e| {
            count += 1;
            if hits.len() < opts.limit {
                hits.push(e as u32);
            }
            !opts.count_total && hits.len() >= opts.limit
        });
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

    /// Length in bytes of the path of entry `i`, or `cap` as soon as it is
    /// known to be at least that long.
    fn path_len_capped(&self, i: usize, cap: u32) -> u32 {
        let mut total = 0u32;
        let mut cur = i;
        for _ in 0..4096 {
            total = total.saturating_add(self.orig_off[cur + 1] - self.orig_off[cur]); // name + the separator
            if total >= cap {
                return cap;
            }
            if self.flags[cur] & FLAG_ROOT != 0 {
                break;
            }
            let p = self.parent[cur];
            match self.lookup(p) {
                Some(next) if p != NONE && next as usize != cur => cur = next as usize,
                _ => break,
            }
        }
        total
    }

    /// Rank class of entry `e` for the folded query terms: 0 = the whole name
    /// (or the name without its extension) is the query, 1 = the name starts
    /// with the first term, 2 = anything else that matched.
    fn rank_class(&self, e: usize, whole: &[u8], first: &[u8]) -> u8 {
        let name = self.lower_name(e);
        if name == whole {
            return 0;
        }
        if let Some(dot) = name.iter().rposition(|&b| b == b'.')
            && dot > 0
            && &name[..dot] == whole
        {
            return 0;
        }
        if name.starts_with(first) { 1 } else { 2 }
    }

    /// Best `limit` matches ordered exact name, then name prefix, then
    /// substring; within a class the shorter path first (then index order).
    /// An empty query returns nothing. `total` counts every match.
    pub fn search_ranked(&self, query: &str, kind: Kind, limit: usize) -> RankedResult {
        let plan = self.plan(query);
        let n = self.len();
        if n == 0 || limit == 0 || plan.driver.is_none() {
            return RankedResult::default();
        }
        // Folded terms in typed order: `whole` is "a b", `first` is "a".
        let folded: Vec<Vec<u8>> = query
            .split_whitespace()
            .map(|t| {
                let mut v = Vec::with_capacity(t.len());
                fold_into(t, &mut v);
                v.retain(|&b| b != 0);
                v
            })
            .filter(|t| !t.is_empty())
            .collect();
        let whole = folded.join(&b' ');
        let first = folded[0].clone();

        let chunk = |e0: usize, e1: usize| -> (usize, Vec<RankedHit>) {
            let mut heap: std::collections::BinaryHeap<RankedHit> = std::collections::BinaryHeap::new();
            let mut count = 0usize;
            self.for_each_match(e0, e1, &plan, kind, &mut |e| {
                count += 1;
                let class = self.rank_class(e, &whole, &first);
                let full = heap.len() >= limit;
                let cap = if full {
                    let worst = heap.peek().unwrap();
                    if class > worst.class {
                        return false;
                    }
                    // Equal class: the path is at least as long as the name, so
                    // a name already as long as the worst path cannot win.
                    if class == worst.class && self.orig_off[e + 1] - self.orig_off[e] > worst.path_len {
                        return false;
                    }
                    if class == worst.class { worst.path_len.saturating_add(1) } else { u32::MAX }
                } else {
                    u32::MAX
                };
                let path_len = self.path_len_capped(e, cap);
                if path_len >= cap && cap != u32::MAX {
                    return false; // longer than the current worst
                }
                let hit = RankedHit { class, path_len, entry: e as u32 };
                if !full {
                    heap.push(hit);
                } else if hit < *heap.peek().unwrap() {
                    heap.pop();
                    heap.push(hit);
                }
                false
            });
            (count, heap.into_vec())
        };

        let chunks = if n < PAR_THRESHOLD { 1 } else { rayon::current_num_threads() * 4 };
        let parts: Vec<(usize, Vec<RankedHit>)> = if chunks == 1 {
            vec![chunk(0, n)]
        } else {
            (0..chunks)
                .into_par_iter()
                .map(|k| chunk(k * n / chunks, (k + 1) * n / chunks))
                .collect()
        };
        let mut total = 0;
        let mut hits = Vec::new();
        for (c, h) in parts {
            total += c;
            hits.extend(h);
        }
        hits.sort_unstable();
        hits.truncate(limit);
        RankedResult { hits, total }
    }
}

/// A ranked match. Ordering is the ranking: smaller is better.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RankedHit {
    pub class: u8,
    pub path_len: u32,
    pub entry: u32,
}

#[derive(Debug, Default)]
pub struct RankedResult {
    pub hits: Vec<RankedHit>,
    pub total: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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

    fn paths(ix: &Index, q: &str) -> Vec<String> {
        let r = ix.search(
            q,
            QueryOpts {
                limit: usize::MAX,
                ..Default::default()
            },
        );
        let mut v: Vec<String> = r.hits.iter().map(|&i| ix.path(i)).collect();
        v.sort();
        v
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

    /// Reference ranking: scan everything, classify and measure the slow way.
    fn naive_ranked(ix: &Index, q: &str, kind: Kind, limit: usize) -> (Vec<RankedHit>, usize) {
        let all = ix.search(q, QueryOpts { limit: usize::MAX, kind, ..Default::default() });
        let mut folded = Vec::new();
        let terms: Vec<&str> = q.split_whitespace().collect();
        for t in &terms {
            let mut v = Vec::new();
            fold_into(t, &mut v);
            folded.push(String::from_utf8(v).unwrap());
        }
        let whole = folded.join(" ");
        let mut v: Vec<RankedHit> = all
            .hits
            .iter()
            .map(|&e| {
                let name = String::from_utf8(ix.lower_name(e as usize).to_vec()).unwrap();
                let stem = name.rsplit_once('.').filter(|(s, _)| !s.is_empty()).map(|(s, _)| s);
                let class = if name == whole || stem == Some(whole.as_str()) {
                    0
                } else if name.starts_with(&folded[0]) {
                    1
                } else {
                    2
                };
                // Walk to the root summing name lengths (+1 each).
                let mut len = 0u32;
                let mut cur = e;
                loop {
                    len += ix.name(cur).len() as u32 + 1;
                    if ix.flags[cur as usize] & FLAG_ROOT != 0 {
                        break;
                    }
                    cur = ix.lookup(ix.parent[cur as usize]).unwrap();
                }
                RankedHit { class, path_len: len, entry: e }
            })
            .collect();
        v.sort();
        v.truncate(limit);
        (v, all.total)
    }

    #[test]
    fn ranked_order_is_exact_then_prefix_then_substring_then_shorter_path() {
        let mut b = Builder::new();
        b.begin_volume("C:", 5);
        b.push(10, 5, "Users", true);
        b.push(11, 10, "Zach", true);
        b.push(12, 11, "my notepad.txt", false); // substring
        b.push(13, 11, "notepad.exe", false); // stem == query -> exact
        b.push(14, 5, "Windows", true);
        b.push(15, 14, "Notepad", true); // exact (case-insensitive)
        b.push(16, 14, "notepad++.exe", false); // prefix
        b.push(17, 5, "notepad-tools", true); // prefix, shorter path
        b.push(18, 11, "Deep", true);
        b.push(19, 18, "notepad.log", false); // exact stem but longest path
        b.end_volume();
        let ix = b.build();
        let r = ix.search_ranked("notepad", Kind::Any, 50);
        let got: Vec<String> = r.hits.iter().map(|h| ix.path(h.entry)).collect();
        assert_eq!(
            got,
            [
                r"C:\Windows\Notepad",
                r"C:\Users\Zach\notepad.exe",
                r"C:\Users\Zach\Deep\notepad.log",
                r"C:\notepad-tools",
                r"C:\Windows\notepad++.exe",
                r"C:\Users\Zach\my notepad.txt",
            ]
        );
        assert_eq!(r.total, 6);
        // Limit keeps the best.
        let r2 = ix.search_ranked("notepad", Kind::Any, 2);
        assert_eq!(r2.hits, r.hits[..2]);
        assert_eq!(r2.total, 6);
        // Nothing for an empty query or limit 0; kind filter applies.
        assert!(ix.search_ranked("  ", Kind::Any, 5).hits.is_empty());
        assert!(ix.search_ranked("notepad", Kind::Any, 0).hits.is_empty());
        let d = ix.search_ranked("notepad", Kind::Dirs, 50);
        assert_eq!(d.hits.len(), 2);
    }

    #[test]
    fn ranked_search_matches_the_naive_ranking_on_a_random_tree() {
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let syl = ["ab", "cd", "ef", "note", "pad", "Foo", "bar", "x", "ÉÀ", "doc"];
        let mut b = Builder::new();
        b.begin_volume("R:", 5);
        let n = 6000u64;
        for i in 0..n {
            let parent = if i < 10 { 5 } else { 100 + rnd(i) };
            let mut name = String::new();
            for _ in 0..1 + rnd(3) {
                name.push_str(syl[rnd(syl.len() as u64) as usize]);
            }
            if rnd(3) == 0 {
                name.push_str([".txt", ".exe", ".md"][rnd(3) as usize]);
            }
            b.push(100 + i, parent, &name, rnd(4) == 0);
        }
        b.end_volume();
        let ix = b.build();
        for q in ["note", "notepad", "ab", "ab cd", "x", "doc.txt", "éà", "foo bar", "zzz", "pad note"] {
            for kind in [Kind::Any, Kind::Files, Kind::Dirs] {
                for limit in [1, 7, 50, 100_000] {
                    let got = ix.search_ranked(q, kind, limit);
                    let (want, total) = naive_ranked(&ix, q, kind, limit);
                    assert_eq!(got.hits, want, "q={q:?} kind={kind:?} limit={limit}");
                    assert_eq!(got.total, total, "q={q:?}");
                }
            }
        }
    }

    #[test]
    fn ranked_parallel_path_agrees_with_serial() {
        let mut b = Builder::new();
        b.begin_volume("T:", 5);
        let n = PAR_THRESHOLD * 2 + 17;
        for i in 0..n {
            let parent = if i < 50 { 5 } else { 100 + (i as u64 * 7) % 50 };
            let name = format!("file{}{}.dat", i, if i % 7 == 0 { "_seven" } else { "" });
            b.push(100 + i as u64, parent, &name, i % 50 == 0);
        }
        b.end_volume();
        let ix = b.build();
        for q in ["seven", "file1", "9_s", "file12345", "zzz"] {
            let got = ix.search_ranked(q, Kind::Any, 30);
            let (want, total) = naive_ranked(&ix, q, Kind::Any, 30);
            assert_eq!(got.hits, want, "q={q}");
            assert_eq!(got.total, total, "q={q}");
        }
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
    fn upsert_adds_renames_and_moves() {
        let mut ix = sample();
        assert_eq!(ix.upsert(20, 11, "Taxes 2026.xlsx", false), Applied::Added);
        assert_eq!(paths(&ix, "taxes"), ["C:\\Users\\Zach\\Taxes 2026.xlsx"]);

        // Unchanged re-apply is a no-op.
        assert_eq!(ix.upsert(20, 11, "Taxes 2026.xlsx", false), Applied::Unchanged);

        // Rename: the old name is gone, the new one is found, same record.
        assert_eq!(ix.upsert(20, 11, "Taxes FINAL.xlsx", false), Applied::Renamed);
        assert!(paths(&ix, "2026").is_empty());
        assert_eq!(paths(&ix, "taxes"), ["C:\\Users\\Zach\\Taxes FINAL.xlsx"]);

        // Move without rename edits the parent only.
        let before = ix.len();
        assert_eq!(ix.upsert(20, 14, "Taxes FINAL.xlsx", false), Applied::Moved);
        assert_eq!(ix.len(), before);
        assert_eq!(paths(&ix, "taxes"), ["C:\\Windows\\Taxes FINAL.xlsx"]);

        assert!(ix.remove(20));
        assert!(!ix.remove(20));
        assert!(paths(&ix, "taxes").is_empty());
        assert_eq!(ix.find_record(20), None);
    }

    #[test]
    fn live_edits_do_not_double_memory() {
        let mut b = Builder::new();
        b.begin_volume("G:", 5);
        for i in 0..200_000u64 {
            b.push(100 + i, 5, &format!("some-file-{i:x}.dat"), false);
        }
        let mut ix = b.build();
        let before = ix.heap_bytes();
        // Plenty of churn: new names, renames (which append), deletes.
        for i in 0..2_000u64 {
            ix.upsert(300_000 + i, 5, &format!("brand-new-{i}.txt"), false);
            ix.upsert(100 + i, 5, &format!("renamed-{i}.dat"), false);
            ix.remove(10_000 + i);
        }
        let after = ix.heap_bytes();
        assert!(after < before + before / 10, "grew from {before} to {after}");
        assert_eq!(ix.search("brand-new-1999", QueryOpts::default()).total, 1);
        // A round trip through compaction also keeps the headroom small.
        let c = ix.compacted();
        assert!(c.heap_bytes() < before + before / 10);
    }

    #[test]
    fn case_only_rename_edits_in_place() {
        let mut ix = sample();
        let before = ix.len();
        assert_eq!(ix.upsert(13, 11, "NOTES.txt", false), Applied::Renamed);
        assert_eq!(ix.len(), before);
        assert_eq!(ix.dead_len(), 0);
        assert_eq!(paths(&ix, "notes"), ["C:\\Users\\Zach\\NOTES.txt"]);
    }

    #[test]
    fn directory_rename_and_move_carry_children() {
        let mut ix = sample();
        assert_eq!(paths(&ix, "notes.txt"), ["C:\\Users\\Zach\\notes.txt"]);
        // Rename Zach -> Zachary: children keep pointing at record 11.
        assert_eq!(ix.upsert(11, 10, "Zachary", true), Applied::Renamed);
        assert_eq!(paths(&ix, "notes.txt"), ["C:\\Users\\Zachary\\notes.txt"]);
        // Move the renamed folder under Windows.
        assert_eq!(ix.upsert(11, 14, "Zachary", true), Applied::Moved);
        assert_eq!(
            paths(&ix, "final"),
            ["C:\\Windows\\Zachary\\Résumé Final.PDF"]
        );
        // A file created later in the renamed folder resolves too.
        ix.upsert(30, 11, "later.txt", false);
        assert_eq!(paths(&ix, "later"), ["C:\\Windows\\Zachary\\later.txt"]);
    }

    #[test]
    fn child_arriving_before_parent_heals() {
        let mut ix = sample();
        ix.upsert(41, 40, "inside.txt", false);
        assert_eq!(paths(&ix, "inside"), ["<orphan>\\inside.txt"]);
        ix.upsert(40, 5, "Late", true);
        assert_eq!(paths(&ix, "inside"), ["C:\\Late\\inside.txt"]);
    }

    #[test]
    fn root_and_garbage_are_ignored() {
        let mut ix = sample();
        assert_eq!(ix.upsert(5, 5, "Hijack", true), Applied::Skipped);
        assert!(!ix.remove(5));
        assert_eq!(ix.upsert(MAX_REC + 1, 5, "huge", false), Applied::Skipped);
        assert_eq!(ix.upsert(50, 5, "", false), Applied::Skipped);
        assert_eq!(ix.path(0), "C:\\");
    }

    #[test]
    fn tombstones_hide_from_every_query_shape() {
        let mut ix = sample();
        ix.remove(13); // notes.txt
        for q in ["notes", "txt", "  ", "note"] {
            let r = ix.search(
                q,
                QueryOpts {
                    limit: usize::MAX,
                    ..Default::default()
                },
            );
            assert!(
                r.hits.iter().all(|&i| ix.name(i) != "notes.txt"),
                "query {q:?}"
            );
        }
        assert_eq!(ix.search("  ", QueryOpts::default()).total, ix.live_len());
        // Multi-term query whose driver would only match the dead name.
        assert_eq!(ix.search("notes zach", QueryOpts::default()).total, 0);
    }

    #[test]
    fn compaction_keeps_results_and_paths() {
        let mut ix = sample();
        ix.upsert(13, 11, "renamed-notes.txt", false);
        ix.remove(15);
        assert!(ix.dead_len() >= 2);
        let live_before = ix.live_len();
        let c = ix.compacted();
        assert_eq!(c.dead_len(), 0);
        assert_eq!(c.len(), live_before);
        for q in ["notes", "exe", "final", "users", "  "] {
            assert_eq!(paths(&ix, q), paths(&c, q), "query {q:?}");
        }
        assert_eq!(c.find_record(13).map(|i| c.name(i).to_string()).as_deref(), Some("renamed-notes.txt"));
        assert_eq!(c.find_record(15), None);
        assert_eq!(c.orphans(), ix.orphans());
    }

    #[test]
    fn reused_record_number_replaces_old_entry() {
        let mut ix = sample();
        assert!(ix.remove(13));
        assert_eq!(ix.upsert(13, 5, "other.bin", false), Applied::Added);
        assert_eq!(paths(&ix, "other"), ["C:\\other.bin"]);
        assert!(paths(&ix, "notes.txt").is_empty());
    }

    #[test]
    fn from_parts_roundtrip_and_rejects_damage() {
        let mut ix = sample();
        ix.remove(13);
        let ix = ix.compacted();
        let p = ix.parts();
        let owned = || OwnedParts {
            lower: p.lower.to_vec(),
            orig: p.orig.to_vec(),
            lower_off: p.lower_off.to_vec(),
            orig_off: p.orig_off.to_vec(),
            parent: p.parent.to_vec(),
            rec: p.rec.to_vec(),
            flags: p.flags.to_vec(),
            orphans: p.orphans,
        };
        let back = Index::from_parts(owned()).unwrap();
        assert_eq!(paths(&back, "  "), paths(&ix, "  "));
        assert_eq!(back.find_record(15), ix.find_record(15));

        let mut bad = owned();
        bad.lower_off[2] = bad.lower_off[1]; // zero-length name
        assert!(Index::from_parts(bad).is_err());
        let mut bad = owned();
        let last = bad.lower.len() - 1;
        bad.lower[last] = b'x'; // lost terminator
        assert!(Index::from_parts(bad).is_err());
        let mut bad = owned();
        bad.rec[3] = bad.rec[2]; // duplicate record number
        assert!(Index::from_parts(bad).is_err());
        let mut bad = owned();
        bad.rec[1] = MAX_REC as u32 + 1;
        assert!(Index::from_parts(bad).is_err());
        let mut bad = owned();
        bad.flags.pop();
        assert!(Index::from_parts(bad).is_err());
    }

    /// Random create/rename/move/delete against a trivial model: every query
    /// must see exactly what the model says exists, before and after compaction.
    #[test]
    fn random_edits_match_a_model() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = |n: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n as u64) as usize
        };
        let words = ["alpha", "beta", "Gamma", "delta", "ÉCLAIR", "log", "tmp", "x"];
        let mut model: HashMap<u64, (u64, String, bool)> = HashMap::new();
        let mut b = Builder::new();
        b.begin_volume("M:", 5);
        b.end_volume();
        let mut ix = b.build();

        let path_of = |model: &HashMap<u64, (u64, String, bool)>, mut rec: u64| -> String {
            let mut parts = Vec::new();
            for _ in 0..64 {
                match model.get(&rec) {
                    Some((p, n, _)) => {
                        parts.push(n.clone());
                        rec = *p;
                    }
                    None if rec == 5 => {
                        parts.push("M:".into());
                        break;
                    }
                    None => {
                        parts.push("<orphan>".into());
                        break;
                    }
                }
            }
            parts.reverse();
            let s = parts.join("\\");
            if s == "M:" { "M:\\".into() } else { s }
        };

        for step in 0..6000 {
            let rec = 6 + rnd(300) as u64;
            match rnd(4) {
                0 | 1 => {
                    let parent = if rnd(3) == 0 { 5 } else { 6 + rnd(300) as u64 };
                    let name = format!("{}{}", words[rnd(words.len())], rnd(40));
                    let is_dir = rnd(4) == 0;
                    // Keep the model free of parent cycles, which no file system produces.
                    let mut cur = parent;
                    let mut cyclic = false;
                    for _ in 0..400 {
                        if cur == rec {
                            cyclic = true;
                            break;
                        }
                        match model.get(&cur) {
                            Some((p, _, _)) => cur = *p,
                            None => break,
                        }
                    }
                    if cyclic || parent == rec {
                        continue;
                    }
                    model.insert(rec, (parent, name.clone(), is_dir));
                    ix.upsert(rec, parent, &name, is_dir);
                }
                _ => {
                    model.remove(&rec);
                    ix.remove(rec);
                }
            }
            if step % 1500 == 1499 {
                ix = ix.compacted();
            }
        }
        for check in 0..2 {
            for w in words.iter().map(|w| w.to_lowercase()).chain(["1".to_string(), "".to_string()]) {
                let mut want: Vec<String> = model
                    .iter()
                    .filter(|(_, (_, n, _))| n.to_lowercase().contains(&w))
                    .map(|(&r, _)| path_of(&model, r))
                    .collect();
                if w.is_empty() {
                    want.push("M:\\".into());
                }
                want.sort();
                assert_eq!(paths(&ix, &w), want, "query {w:?} (check {check})");
            }
            ix = ix.compacted();
        }
    }
    #[test]
    fn from_parts_rejects_offsets_past_the_buffer_without_panicking() {
        let bad = OwnedParts {
            lower: b"c:\0a\0".to_vec(),
            orig: b"C:\0a\0".to_vec(),
            lower_off: vec![0, 900, 5],
            orig_off: vec![0, 3, 5],
            parent: vec![NONE, 0],
            rec: vec![5, 6],
            flags: vec![FLAG_DIR | FLAG_ROOT, 0],
            orphans: 0,
        };
        assert!(Index::from_parts(bad).is_err());
    }

}
