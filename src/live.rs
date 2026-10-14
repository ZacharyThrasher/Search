//! Keeps one in-RAM `Index` per volume current.
//!
//! Start-up, per volume:
//!
//! 1. Load the saved snapshot, if any, and ask the volume for its USN journal.
//! 2. If the journal is the same one the snapshot was taken from and still
//!    covers the saved position, replay just the records since then.
//! 3. Otherwise (no snapshot, journal reset, journal wrapped, snapshot
//!    unreadable) do a full scan. The journal position is read *before* the
//!    scan starts and replayed afterwards; replay is idempotent, so changes
//!    that raced the scan are applied twice at worst.
//!
//! Then a tail thread polls the journal every `Config::poll` and edits the
//! index in place. If the tail ever finds the journal gone or wrapped it
//! rescans in the background while the old index keeps answering queries.
//!
//! The volume itself sits behind the `Source` trait (the NTFS implementation
//! is in `mft`), so everything here is exercised by tests with a fake.

use std::fmt;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::fmt::{age, commas, dur, mib};
use crate::index::{Applied, Index, Kind, QueryOpts};
use crate::query::Query;
use crate::snapshot::{self, LoadError, Meta};
use crate::usn::{REASON_FILE_CREATE, REASON_FILE_DELETE, REASON_RENAME_NEW_NAME};

/// Where a volume's journal stands right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalInfo {
    pub id: u64,
    pub first_usn: i64,
    pub lowest_valid_usn: i64,
    pub next_usn: i64,
}

/// One journal record that matters to the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub usn: i64,
    pub reason: u32,
    pub record: u64,
    pub parent: u64,
    pub is_dir: bool,
    pub name: String,
}

#[derive(Debug)]
pub enum SourceError {
    /// The volume has no USN journal. We never create one.
    NoJournal,
    /// Raw volume access needs an elevated process.
    AccessDenied,
    Io(io::Error),
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::NoJournal => write!(f, "USN journal is not active on this volume"),
            SourceError::AccessDenied => write!(f, "access denied (needs an elevated shell)"),
            SourceError::Io(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Debug)]
pub enum ReadError {
    /// The journal was reset, deleted, or wrapped past where we are reading.
    /// The index can no longer be brought current by replay.
    Gone,
    Io(io::Error),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Gone => write!(f, "journal reset or wrapped"),
            ReadError::Io(e) => write!(f, "{e}"),
        }
    }
}

pub struct Scan {
    pub index: Index,
    /// One log line of timings for the scan.
    pub detail: String,
}

/// A volume the engine can scan, snapshot-resume and tail.
pub trait Source: Send + Sync {
    /// Shown in paths and logs, e.g. `C:`.
    fn label(&self) -> String;
    /// Snapshot file stem, e.g. `C`.
    fn key(&self) -> String;
    /// Current journal state. Must not create a journal.
    fn journal(&self) -> Result<JournalInfo, SourceError>;
    /// Enumerate every file into a fresh index.
    fn scan(&self) -> Result<Scan, SourceError>;
    /// Append the changes at or after `from` to `out` and return the USN to
    /// continue from. Returns `ReadError::Gone` if `journal_id` is not the
    /// active journal or `from` is no longer covered.
    fn read(&self, journal_id: u64, from: i64, out: &mut Vec<Change>) -> Result<i64, ReadError>;
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ApplyStats {
    pub added: usize,
    pub renamed: usize,
    pub moved: usize,
    pub removed: usize,
    pub ignored: usize,
}

impl ApplyStats {
    pub fn total(&self) -> usize {
        self.added + self.renamed + self.moved + self.removed + self.ignored
    }
}

/// Apply one journal record. Deletes win over everything else in the record
/// (a file created and deleted in one handle session shows both bits); a
/// create or rename-to sets the name; anything else is not our business.
/// Applying the same record twice, or in a state that already reflects it,
/// leaves the index unchanged.
pub fn apply(index: &mut Index, c: &Change, stats: &mut ApplyStats) {
    if c.reason & REASON_FILE_DELETE != 0 {
        if index.remove(c.record) {
            stats.removed += 1;
        } else {
            stats.ignored += 1;
        }
    } else if c.reason & (REASON_FILE_CREATE | REASON_RENAME_NEW_NAME) != 0 {
        match index.upsert(c.record, c.parent, &c.name, c.is_dir) {
            Applied::Added => stats.added += 1,
            Applied::Renamed => stats.renamed += 1,
            Applied::Moved => stats.moved += 1,
            Applied::Unchanged | Applied::Skipped => stats.ignored += 1,
        }
    } else {
        stats.ignored += 1;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Resume {
    Yes,
    Rescan(&'static str),
}

/// Can a snapshot taken at `saved` be brought current by replaying `now`?
pub fn decide_resume(saved: &Meta, now: &JournalInfo) -> Resume {
    if saved.journal_id != now.id {
        Resume::Rescan("journal was reset since the snapshot")
    } else if saved.next_usn < now.first_usn.max(now.lowest_valid_usn) {
        Resume::Rescan("journal wrapped past the snapshot")
    } else if saved.next_usn > now.next_usn {
        Resume::Rescan("snapshot is ahead of the journal")
    } else {
        Resume::Yes
    }
}

/// When a snapshot is worth rewriting. Replay makes a stale snapshot cheap,
/// so writes (hundreds of MB on a big volume) are rationed.
#[derive(Debug, Clone, Copy)]
pub struct SavePolicy {
    pub min_changes: u64,
    pub max_age: Duration,
    pub force: bool,
}

impl Default for SavePolicy {
    fn default() -> Self {
        SavePolicy {
            min_changes: 20_000,
            max_age: Duration::from_secs(12 * 3600),
            force: false,
        }
    }
}

pub struct Config {
    /// Snapshots live here. `None` disables loading and saving.
    pub data_dir: Option<PathBuf>,
    /// Keep a thread per volume tailing the journal.
    pub tail: bool,
    /// Ignore any existing snapshot and rescan.
    pub force_rescan: bool,
    /// How often the tail polls an idle journal.
    pub poll: Duration,
    /// Rewrite snapshots on this cadence (subject to `SavePolicy`). `None` = only on request.
    pub autosave: Option<Duration>,
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            data_dir: None,
            tail: true,
            force_rescan: false,
            poll: Duration::from_millis(100),
            autosave: Some(Duration::from_secs(6 * 3600)),
            log: Arc::new(|_| {}),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    NoJournal,
    AccessDenied,
    Other,
}

/// A volume that could not be brought up.
#[derive(Debug, Clone)]
pub struct Failure {
    pub label: String,
    pub kind: FailureKind,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub label: String,
    pub entries: usize,
    pub dead: usize,
    pub heap_bytes: usize,
    /// Tail thread is running.
    pub live: bool,
    /// Served from an old snapshot because the journal could not be read.
    pub stale: bool,
    /// Journal records applied since start.
    pub applied: u64,
    pub snapshot_age: Option<Duration>,
}

struct State {
    index: Index,
    journal_id: u64,
}

struct Volume {
    label: String,
    source: Option<Arc<dyn Source>>,
    data_dir: Option<PathBuf>,
    stale: bool,
    state: RwLock<State>,
    /// Journal position the index is current to. Advanced under the write lock
    /// when records are applied, so a reader holding the read lock sees an
    /// (index, position) pair that belong together.
    next_usn: AtomicI64,
    live: AtomicBool,
    applied: AtomicU64,
    since_save: AtomicU64,
    /// The snapshot on disk, if any.
    saved: Mutex<Option<Meta>>,
    /// Serializes snapshot writes (they share one temp file).
    save_lock: Mutex<()>,
    /// Taken by anything that needs the write lock on `state`, and by a
    /// snapshot write for its whole duration. A writer queued on an `RwLock`
    /// holds back every new reader, so without this a snapshot (a long read)
    /// plus a tail batch (a short write) would freeze queries until the
    /// snapshot finished. With it, the tail waits here instead and the lock
    /// is never contended by a pending writer, so queries never block.
    write_gate: Mutex<()>,
    /// Test hook: pretend writing the snapshot takes this long.
    #[cfg(test)]
    slow_save: Mutex<Duration>,
}

fn gate(m: &Mutex<()>) -> MutexGuard<'_, ()> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn rd<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

fn wr<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

impl Volume {
    fn snapshot_path(&self) -> Option<PathBuf> {
        let dir = self.data_dir.as_ref()?;
        let key = self.source.as_ref()?.key();
        Some(snapshot::path_for(dir, &key))
    }

    fn should_save(&self, p: &SavePolicy) -> bool {
        if self.stale || self.snapshot_path().is_none() {
            return false;
        }
        if p.force {
            return true;
        }
        match *self.saved.lock().unwrap_or_else(|e| e.into_inner()) {
            None => true,
            Some(m) => {
                self.since_save.load(Ordering::Relaxed) >= p.min_changes || m.age() >= p.max_age
            }
        }
    }

    /// Write the snapshot. Holds only the read lock (and the write gate, which
    /// keeps the tail from queueing a writer behind it), so queries continue and
    /// the tail's next batch waits.
    fn save(&self) -> io::Result<(u64, Duration)> {
        let Some(path) = self.snapshot_path() else {
            return Ok((0, Duration::ZERO));
        };
        let _one_writer = self.save_lock.lock().unwrap_or_else(|e| e.into_inner());
        let _no_writers = gate(&self.write_gate);
        let t = Instant::now();
        let st = rd(&self.state);
        #[cfg(test)]
        thread::sleep(*self.slow_save.lock().unwrap());
        let meta = Meta::now(st.journal_id, self.next_usn.load(Ordering::Acquire));
        let bytes = snapshot::save(&path, &meta, &st.index)?;
        *self.saved.lock().unwrap_or_else(|e| e.into_inner()) = Some(meta);
        self.since_save.store(0, Ordering::Relaxed);
        drop(st);
        Ok((bytes, t.elapsed()))
    }
}

/// Replay journal records from `*usn` until none are left.
fn catch_up(
    source: &dyn Source,
    st: &mut State,
    usn: &mut i64,
) -> Result<ApplyStats, ReadError> {
    let mut buf = Vec::new();
    let mut total = ApplyStats::default();
    loop {
        buf.clear();
        let next = source.read(st.journal_id, *usn, &mut buf)?;
        for c in &buf {
            apply(&mut st.index, c, &mut total);
        }
        let progressed = next > *usn;
        *usn = (*usn).max(next);
        if buf.is_empty() || !progressed {
            return Ok(total);
        }
    }
}

type Log = dyn Fn(&str) + Send + Sync;

/// Full scan plus replay of whatever changed while it ran.
fn full_scan(source: &dyn Source, label: &str, log: &Log) -> Result<(State, i64, ApplyStats), SourceError> {
    // The position is taken first: anything that happens during the scan has
    // a later USN and is replayed below.
    let j0 = source.journal()?;
    let scan = source.scan()?;
    log(&format!("  {label} {}", scan.detail));
    let mut st = State {
        index: scan.index,
        journal_id: j0.id,
    };
    let mut usn = j0.next_usn;
    let t = Instant::now();
    let stats = catch_up(source, &mut st, &mut usn).map_err(|e| match e {
        ReadError::Gone => SourceError::Io(io::Error::other(
            "journal changed underneath the scan; try again",
        )),
        ReadError::Io(e) => SourceError::Io(e),
    })?;
    if stats.total() > 0 {
        log(&format!(
            "  {label} replayed {} journal records that raced the scan in {}",
            commas(stats.total()),
            dur(t.elapsed())
        ));
    }
    Ok((st, usn, stats))
}

fn open_volume(source: &Arc<dyn Source>, cfg: &Config) -> Result<Volume, SourceError> {
    let label = source.label();
    let log = &*cfg.log;
    let snap_path = cfg
        .data_dir
        .as_ref()
        .map(|d| snapshot::path_for(d, &source.key()));

    let mut loaded: Option<(Meta, Index)> = None;
    if let (Some(p), false) = (&snap_path, cfg.force_rescan) {
        let t = Instant::now();
        match snapshot::load(p) {
            Ok((m, ix)) => {
                log(&format!(
                    "  {label} snapshot: {} entries, {}, loaded in {}",
                    commas(ix.live_len()),
                    mib(ix.heap_bytes()),
                    dur(t.elapsed())
                ));
                loaded = Some((m, ix));
            }
            Err(LoadError::Missing) => {}
            Err(e) => log(&format!("  {label} {e}")),
        }
    }

    let mk = |st: State, usn: i64, saved: Option<Meta>, since_save: u64, stale: bool| Volume {
        label: label.clone(),
        source: Some(source.clone()),
        data_dir: cfg.data_dir.clone(),
        stale,
        state: RwLock::new(st),
        next_usn: AtomicI64::new(usn),
        live: AtomicBool::new(false),
        applied: AtomicU64::new(since_save),
        since_save: AtomicU64::new(since_save),
        saved: Mutex::new(saved),
        save_lock: Mutex::new(()),
        write_gate: Mutex::new(()),
        #[cfg(test)]
        slow_save: Mutex::new(Duration::ZERO),
    };

    let journal = match source.journal() {
        Ok(j) => j,
        // Can't read the journal, but an old snapshot is better than nothing.
        Err(SourceError::AccessDenied) if loaded.is_some() => {
            let (meta, index) = loaded.unwrap();
            log(&format!(
                "  {label} not elevated: serving the snapshot from {} ago, no live updates",
                age(meta.age())
            ));
            let st = State {
                index,
                journal_id: meta.journal_id,
            };
            return Ok(mk(st, meta.next_usn, Some(meta), 0, true));
        }
        Err(e) => return Err(e),
    };

    if let Some((meta, index)) = loaded {
        match decide_resume(&meta, &journal) {
            Resume::Yes => {
                let mut st = State {
                    index,
                    journal_id: meta.journal_id,
                };
                let mut usn = meta.next_usn;
                let t = Instant::now();
                match catch_up(&**source, &mut st, &mut usn) {
                    Ok(stats) => {
                        log(&format!(
                            "  {label} caught up {} journal records in {} (snapshot is {} old)",
                            commas(stats.total()),
                            dur(t.elapsed()),
                            age(meta.age())
                        ));
                        return Ok(mk(st, usn, Some(meta), stats.total() as u64, false));
                    }
                    Err(ReadError::Gone) => {
                        log(&format!("  {label} journal no longer covers the snapshot; rescanning"))
                    }
                    Err(ReadError::Io(e)) => return Err(SourceError::Io(e)),
                }
            }
            Resume::Rescan(why) => log(&format!("  {label} {why}; rescanning")),
        }
    }

    let (st, usn, stats) = full_scan(&**source, &label, log)?;
    Ok(mk(st, usn, None, stats.total() as u64, false))
}

pub struct Hit {
    /// Index into `Engine::volumes()`.
    pub volume: usize,
    pub path: String,
    pub is_dir: bool,
}

pub struct SearchResult {
    pub hits: Vec<Hit>,
    pub total: usize,
}

pub struct Engine {
    vols: Vec<Arc<Volume>>,
    /// Volumes that could not be brought up (and why).
    pub failures: Vec<Failure>,
    stop: Arc<AtomicBool>,
    tails: Vec<JoinHandle<()>>,
    cfg: Arc<Config>,
}

impl Engine {
    /// Bring up every source (in parallel) and, if `cfg.tail`, start tailing.
    pub fn start(sources: Vec<Arc<dyn Source>>, cfg: Config) -> Engine {
        let cfg = Arc::new(cfg);
        let results: Vec<(String, Result<Volume, SourceError>)> = thread::scope(|s| {
            let handles: Vec<_> = sources
                .iter()
                .map(|src| {
                    let cfg = &cfg;
                    s.spawn(move || (src.label(), open_volume(src, cfg)))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut vols = Vec::new();
        let mut failures = Vec::new();
        for (label, r) in results {
            match r {
                Ok(v) => vols.push(Arc::new(v)),
                Err(e) => failures.push(Failure {
                    label,
                    kind: match e {
                        SourceError::NoJournal => FailureKind::NoJournal,
                        SourceError::AccessDenied => FailureKind::AccessDenied,
                        SourceError::Io(_) => FailureKind::Other,
                    },
                    message: e.to_string(),
                }),
            }
        }
        let mut engine = Engine {
            vols,
            failures,
            stop: Arc::new(AtomicBool::new(false)),
            tails: Vec::new(),
            cfg,
        };
        if engine.cfg.tail {
            for v in engine.vols.iter().filter(|v| !v.stale) {
                v.live.store(true, Ordering::Release);
                let (v, stop, cfg) = (v.clone(), engine.stop.clone(), engine.cfg.clone());
                engine.tails.push(
                    thread::Builder::new()
                        .name(format!("tail-{}", v.label))
                        .spawn(move || {
                            let r = panic::catch_unwind(AssertUnwindSafe(|| tail(&v, &stop, &cfg)));
                            if r.is_err() {
                                (cfg.log)(&format!("  {} live updates stopped: internal error", v.label));
                            }
                            v.live.store(false, Ordering::Release);
                        })
                        .expect("spawn tail thread"),
                );
            }
        }
        engine
    }

    /// Indexes that are fixed for the life of the process (directory walks).
    pub fn from_indexes(indexes: Vec<(String, Index)>) -> Engine {
        let vols = indexes
            .into_iter()
            .map(|(label, index)| {
                Arc::new(Volume {
                    label,
                    source: None,
                    data_dir: None,
                    stale: false,
                    state: RwLock::new(State { index, journal_id: 0 }),
                    next_usn: AtomicI64::new(0),
                    live: AtomicBool::new(false),
                    applied: AtomicU64::new(0),
                    since_save: AtomicU64::new(0),
                    saved: Mutex::new(None),
                    save_lock: Mutex::new(()),
                    write_gate: Mutex::new(()),
                    #[cfg(test)]
                    slow_save: Mutex::new(Duration::ZERO),
                })
            })
            .collect();
        Engine {
            vols,
            failures: Vec::new(),
            stop: Arc::new(AtomicBool::new(false)),
            tails: Vec::new(),
            cfg: Arc::new(Config::default()),
        }
    }

    pub fn volumes(&self) -> Vec<VolumeInfo> {
        self.vols
            .iter()
            .map(|v| {
                let st = rd(&v.state);
                VolumeInfo {
                    label: v.label.clone(),
                    entries: st.index.live_len(),
                    dead: st.index.dead_len(),
                    heap_bytes: st.index.heap_bytes(),
                    live: v.live.load(Ordering::Acquire),
                    stale: v.stale,
                    applied: v.applied.load(Ordering::Relaxed),
                    snapshot_age: (*v.saved.lock().unwrap_or_else(|e| e.into_inner())).map(|m| m.age()),
                }
            })
            .collect()
    }

    /// Total live entries across volumes.
    pub fn len(&self) -> usize {
        self.vols.iter().map(|v| rd(&v.state).index.live_len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn heap_bytes(&self) -> usize {
        self.vols.iter().map(|v| rd(&v.state).index.heap_bytes()).sum()
    }

    /// Search every volume, in volume order. Paths are built while the volume
    /// is still locked, so they always match the hits.
    pub fn search(&self, query: &str, opts: QueryOpts) -> SearchResult {
        let mut hits: Vec<Hit> = Vec::new();
        let mut total = 0;
        let q = Query::parse(query);
        for (vi, v) in self.vols.iter().enumerate() {
            if !opts.count_total && hits.len() >= opts.limit {
                break;
            }
            let st = rd(&v.state);
            let r = st.index.search_query(
                &q,
                QueryOpts {
                    limit: opts.limit.saturating_sub(hits.len()),
                    ..opts
                },
            );
            total += r.total;
            hits.extend(r.hits.iter().map(|&h| Hit {
                volume: vi,
                path: st.index.path(h),
                is_dir: st.index.is_dir(h),
            }));
        }
        SearchResult { hits, total }
    }

    /// Ranked search across volumes: exact name, then name prefix, then
    /// substring; shorter paths first within a class. `total` counts every
    /// match. Paths are built under the volume lock, like `search`.
    pub fn search_ranked(&self, query: &str, kind: Kind, limit: usize) -> SearchResult {
        let mut cands: Vec<(u8, u32, usize, Hit)> = Vec::new();
        let mut total = 0;
        let q = Query::parse(query);
        for (vi, v) in self.vols.iter().enumerate() {
            let st = rd(&v.state);
            let r = st.index.search_ranked_query(&q, kind, limit);
            total += r.total;
            cands.extend(r.hits.iter().map(|h| {
                let hit = Hit {
                    volume: vi,
                    path: st.index.path(h.entry),
                    is_dir: st.index.is_dir(h.entry),
                };
                (h.class, h.path_len, vi, hit)
            }));
        }
        if q.is_flat() {
            // No exact/prefix classes: shorter path, then file name, then volume.
            let name = |h: &Hit| h.path.rsplit('\\').next().unwrap_or("").to_lowercase();
            cands.sort_by_cached_key(|c| (c.0, c.1, name(&c.3), c.2));
        } else {
            cands.sort_by_key(|c| (c.0, c.1, c.2));
        }
        cands.truncate(limit);
        SearchResult {
            hits: cands.into_iter().map(|c| c.3).collect(),
            total,
        }
    }

    /// Stop the tail threads (and wait for them).
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for t in &self.tails {
            t.thread().unpark();
        }
        for t in self.tails.drain(..) {
            let _ = t.join();
        }
    }

    /// Write snapshots for the volumes `policy` says are worth it. Returns how
    /// many were written. Failures are logged, not fatal.
    pub fn save(&self, policy: SavePolicy) -> usize {
        let mut n = 0;
        for v in &self.vols {
            if !v.should_save(&policy) {
                continue;
            }
            match v.save() {
                Ok((bytes, took)) => {
                    n += 1;
                    (self.cfg.log)(&format!(
                        "  {} snapshot saved: {} in {}",
                        v.label,
                        mib(bytes as usize),
                        dur(took)
                    ));
                }
                Err(e) => (self.cfg.log)(&format!("  {} snapshot not saved: {e}", v.label)),
            }
        }
        n
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

fn nap(d: Duration, stop: &AtomicBool) {
    if !stop.load(Ordering::Acquire) {
        thread::park_timeout(d);
    }
}

fn tail(v: &Volume, stop: &AtomicBool, cfg: &Config) {
    let Some(source) = v.source.as_ref() else {
        return;
    };
    let log = &*cfg.log;
    let mut buf: Vec<Change> = Vec::new();
    let mut last_save = Instant::now();
    // Rescans since the last successful read; a journal that is "gone" again
    // right after a fresh scan is not going to get better by scanning more.
    let mut rescans = 0u32;

    while !stop.load(Ordering::Acquire) {
        let id = rd(&v.state).journal_id;
        let from = v.next_usn.load(Ordering::Acquire);
        buf.clear();
        match source.read(id, from, &mut buf) {
            Ok(next) => {
                rescans = 0;
                if buf.is_empty() {
                    v.next_usn.fetch_max(next, Ordering::AcqRel);
                    nap(cfg.poll, stop);
                } else {
                    let mut stats = ApplyStats::default();
                    {
                        let _g = gate(&v.write_gate);
                        let mut st = wr(&v.state);
                        for c in &buf {
                            apply(&mut st.index, c, &mut stats);
                        }
                        v.next_usn.fetch_max(next, Ordering::AcqRel);
                    }
                    let n = stats.total() as u64;
                    v.applied.fetch_add(n, Ordering::Relaxed);
                    v.since_save.fetch_add(n, Ordering::Relaxed);
                    if rd(&v.state).index.needs_compaction() {
                        // Only this thread writes, so the copy cannot go stale.
                        let t = Instant::now();
                        let fresh = rd(&v.state).index.compacted();
                        let dead = rd(&v.state).index.dead_len();
                        let _g = gate(&v.write_gate);
                        wr(&v.state).index = fresh;
                        log(&format!(
                            "  {} compacted {} dead entries in {}",
                            v.label,
                            commas(dead),
                            dur(t.elapsed())
                        ));
                    }
                }
            }
            Err(ReadError::Gone) => {
                rescans += 1;
                if rescans > 3 {
                    log(&format!(
                        "  {} journal keeps resetting; live updates stopped (results may go stale)",
                        v.label
                    ));
                    return;
                }
                log(&format!("  {} journal reset or wrapped; rescanning in the background", v.label));
                match full_scan(&**source, &v.label, log) {
                    Ok((fresh, usn, stats)) => {
                        let g = gate(&v.write_gate);
                        let mut st = wr(&v.state);
                        *st = fresh;
                        v.next_usn.store(usn, Ordering::Release);
                        drop(st);
                        drop(g);
                        // The snapshot on disk now belongs to a dead journal.
                        *v.saved.lock().unwrap_or_else(|e| e.into_inner()) = None;
                        v.since_save.store(stats.total() as u64, Ordering::Relaxed);
                        last_save = Instant::now();
                        v.save().ok();
                    }
                    Err(e) => {
                        log(&format!("  {} rescan failed: {e}", v.label));
                        nap(Duration::from_secs(5), stop);
                    }
                }
            }
            Err(ReadError::Io(e)) => {
                log(&format!("  {} journal read failed: {e}", v.label));
                nap(Duration::from_secs(1), stop);
            }
        }

        if let Some(every) = cfg.autosave
            && last_save.elapsed() >= every
        {
            last_save = Instant::now();
            if v.should_save(&SavePolicy::default()) {
                match v.save() {
                    Ok((bytes, took)) => log(&format!(
                        "  {} snapshot saved: {} in {}",
                        v.label,
                        mib(bytes as usize),
                        dur(took)
                    )),
                    Err(e) => log(&format!("  {} snapshot not saved: {e}", v.label)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Builder;
    use std::collections::BTreeMap;

    type Node = (u64, String, bool);

    struct Fs {
        files: BTreeMap<u64, Node>,
        journal: Vec<Change>,
        id: u64,
        first_usn: i64,
        next_usn: i64,
        scans: usize,
        deny: bool,
        no_journal: bool,
        /// Mutations applied inside `scan`, after the journal position was
        /// taken, to model changes racing the enumeration.
        during_scan: Vec<(u64, Option<Node>)>,
    }

    struct Fake {
        label: String,
        fs: Mutex<Fs>,
    }

    impl Fake {
        fn new(label: &str) -> Arc<Fake> {
            let mut files = BTreeMap::new();
            files.insert(10, (5, "Users".to_string(), true));
            files.insert(11, (10, "Zach".to_string(), true));
            files.insert(12, (11, "notes.txt".to_string(), false));
            Arc::new(Fake {
                label: label.into(),
                fs: Mutex::new(Fs {
                    files,
                    journal: Vec::new(),
                    id: 77,
                    first_usn: 0,
                    next_usn: 1000,
                    scans: 0,
                    deny: false,
                    no_journal: false,
                    during_scan: Vec::new(),
                }),
            })
        }

        fn change(fs: &mut Fs, reason: u32, rec: u64, parent: u64, name: &str, is_dir: bool) {
            let usn = fs.next_usn;
            fs.next_usn += 96;
            fs.journal.push(Change {
                usn,
                reason,
                record: rec,
                parent,
                is_dir,
                name: name.into(),
            });
        }

        fn create(&self, rec: u64, parent: u64, name: &str, is_dir: bool) {
            let mut fs = self.fs.lock().unwrap();
            fs.files.insert(rec, (parent, name.into(), is_dir));
            Self::change(&mut fs, REASON_FILE_CREATE, rec, parent, name, is_dir);
        }

        fn rename(&self, rec: u64, parent: u64, name: &str) {
            let mut fs = self.fs.lock().unwrap();
            let is_dir = fs.files[&rec].2;
            fs.files.insert(rec, (parent, name.into(), is_dir));
            Self::change(&mut fs, REASON_RENAME_NEW_NAME, rec, parent, name, is_dir);
        }

        fn delete(&self, rec: u64) {
            let mut fs = self.fs.lock().unwrap();
            let (parent, name, is_dir) = fs.files.remove(&rec).unwrap();
            Self::change(&mut fs, REASON_FILE_DELETE, rec, parent, &name, is_dir);
        }

        fn wrap_journal(&self) {
            let mut fs = self.fs.lock().unwrap();
            fs.journal.clear();
            fs.first_usn = fs.next_usn;
        }

        fn reset_journal(&self) {
            let mut fs = self.fs.lock().unwrap();
            fs.journal.clear();
            fs.id += 1;
            fs.first_usn = fs.next_usn;
        }

        fn scans(&self) -> usize {
            self.fs.lock().unwrap().scans
        }
    }

    impl Source for Fake {
        fn label(&self) -> String {
            self.label.clone()
        }

        fn key(&self) -> String {
            self.label.trim_end_matches(':').to_string()
        }

        fn journal(&self) -> Result<JournalInfo, SourceError> {
            let fs = self.fs.lock().unwrap();
            if fs.deny {
                return Err(SourceError::AccessDenied);
            }
            if fs.no_journal {
                return Err(SourceError::NoJournal);
            }
            Ok(JournalInfo {
                id: fs.id,
                first_usn: fs.first_usn,
                lowest_valid_usn: fs.first_usn,
                next_usn: fs.next_usn,
            })
        }

        fn scan(&self) -> Result<Scan, SourceError> {
            let mut fs = self.fs.lock().unwrap();
            fs.scans += 1;
            for (rec, to) in std::mem::take(&mut fs.during_scan) {
                match to {
                    Some((parent, name, is_dir)) => {
                        let reason = if fs.files.contains_key(&rec) {
                            REASON_RENAME_NEW_NAME
                        } else {
                            REASON_FILE_CREATE
                        };
                        fs.files.insert(rec, (parent, name.clone(), is_dir));
                        Self::change(&mut fs, reason, rec, parent, &name, is_dir);
                    }
                    None => {
                        let (parent, name, is_dir) = fs.files.remove(&rec).unwrap();
                        Self::change(&mut fs, REASON_FILE_DELETE, rec, parent, &name, is_dir);
                    }
                }
            }
            let mut b = Builder::new();
            b.begin_volume(&self.label, 5);
            for (&rec, (parent, name, is_dir)) in &fs.files {
                b.push(rec, *parent, name, *is_dir);
            }
            Ok(Scan {
                index: b.build(),
                detail: format!("scanned {} files", fs.files.len()),
            })
        }

        fn read(&self, journal_id: u64, from: i64, out: &mut Vec<Change>) -> Result<i64, ReadError> {
            let fs = self.fs.lock().unwrap();
            if journal_id != fs.id || from < fs.first_usn || from > fs.next_usn {
                return Err(ReadError::Gone);
            }
            out.extend(fs.journal.iter().filter(|c| c.usn >= from).cloned());
            Ok(fs.next_usn)
        }
    }

    fn cfg(dir: Option<&std::path::Path>) -> Config {
        Config {
            data_dir: dir.map(|d| d.to_path_buf()),
            tail: false,
            poll: Duration::from_millis(5),
            ..Default::default()
        }
    }

    fn start(srcs: &[&Arc<Fake>], c: Config) -> Engine {
        Engine::start(srcs.iter().map(|s| (*s).clone() as Arc<dyn Source>).collect(), c)
    }

    fn all(e: &Engine) -> Vec<String> {
        let r = e.search(
            "",
            QueryOpts {
                limit: usize::MAX,
                ..Default::default()
            },
        );
        let mut v: Vec<String> = r.hits.into_iter().map(|h| h.path).collect();
        v.sort();
        v
    }

    fn find(e: &Engine, q: &str) -> Vec<String> {
        let r = e.search(
            q,
            QueryOpts {
                limit: usize::MAX,
                ..Default::default()
            },
        );
        let mut v: Vec<String> = r.hits.into_iter().map(|h| h.path).collect();
        v.sort();
        v
    }

    #[test]
    fn resume_decisions() {
        let j = JournalInfo {
            id: 9,
            first_usn: 100,
            lowest_valid_usn: 100,
            next_usn: 500,
        };
        let m = |id, usn| Meta {
            journal_id: id,
            next_usn: usn,
            saved_unix: 0,
        };
        assert_eq!(decide_resume(&m(9, 100), &j), Resume::Yes);
        assert_eq!(decide_resume(&m(9, 300), &j), Resume::Yes);
        assert_eq!(decide_resume(&m(9, 500), &j), Resume::Yes);
        assert!(matches!(decide_resume(&m(8, 300), &j), Resume::Rescan(_)));
        assert!(matches!(decide_resume(&m(9, 99), &j), Resume::Rescan(_)));
        assert!(matches!(decide_resume(&m(9, 501), &j), Resume::Rescan(_)));
        // lowest_valid above first_usn also forces a rescan.
        let j2 = JournalInfo {
            lowest_valid_usn: 200,
            ..j
        };
        assert!(matches!(decide_resume(&m(9, 150), &j2), Resume::Rescan(_)));
    }

    #[test]
    fn apply_semantics() {
        let mut b = Builder::new();
        b.begin_volume("T:", 5);
        let mut ix = b.build();
        let mut s = ApplyStats::default();
        let ch = |reason, name: &str| Change {
            usn: 0,
            reason,
            record: 20,
            parent: 5,
            is_dir: false,
            name: name.into(),
        };
        apply(&mut ix, &ch(REASON_FILE_CREATE | 0x2, "a.txt"), &mut s);
        apply(&mut ix, &ch(REASON_FILE_CREATE, "a.txt"), &mut s); // replay
        apply(&mut ix, &ch(0x1000, "a.txt"), &mut s); // rename-old: ignored
        apply(&mut ix, &ch(REASON_RENAME_NEW_NAME | 0x8000_0000, "b.txt"), &mut s);
        assert_eq!((s.added, s.renamed, s.ignored), (1, 1, 2));
        // Created and deleted within one handle session: gone.
        apply(&mut ix, &ch(REASON_FILE_CREATE | REASON_FILE_DELETE, "b.txt"), &mut s);
        assert_eq!(s.removed, 1);
        assert_eq!(ix.find_record(20), None);
        apply(&mut ix, &ch(REASON_FILE_DELETE, "b.txt"), &mut s); // replayed delete
        assert_eq!(s.removed, 1);
    }

    #[test]
    fn cold_start_scans_and_second_start_loads_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("T1:");
        {
            let e = start(&[&fake], cfg(Some(dir.path())));
            assert!(e.failures.is_empty());
            assert_eq!(fake.scans(), 1);
            assert_eq!(find(&e, "notes"), ["T1:\\Users\\Zach\\notes.txt"]);
            assert_eq!(e.save(SavePolicy::default()), 1); // never saved: always writes
            assert_eq!(e.save(SavePolicy::default()), 0); // nothing new
        }
        let e = start(&[&fake], cfg(Some(dir.path())));
        assert_eq!(fake.scans(), 1, "restart must not rescan");
        assert_eq!(find(&e, "notes"), ["T1:\\Users\\Zach\\notes.txt"]);
        assert!(e.volumes()[0].snapshot_age.is_some());
    }

    #[test]
    fn changes_made_while_stopped_are_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("T1:");
        start(&[&fake], cfg(Some(dir.path()))).save(SavePolicy::default());

        // While no engine is running: create, rename, move, delete, rename a dir.
        fake.create(20, 11, "fresh.txt", false);
        fake.create(21, 11, "temp.tmp", false);
        fake.rename(12, 10, "moved-notes.txt");
        fake.delete(21);
        fake.rename(11, 10, "Zachary");

        let e = start(&[&fake], cfg(Some(dir.path())));
        assert_eq!(fake.scans(), 1);
        assert_eq!(find(&e, "fresh"), ["T1:\\Users\\Zachary\\fresh.txt"]);
        assert_eq!(find(&e, "notes"), ["T1:\\Users\\moved-notes.txt"]);
        assert!(find(&e, "temp").is_empty());
        let mut want: Vec<String> = ["T1:\\", "T1:\\Users", "T1:\\Users\\Zachary", "T1:\\Users\\Zachary\\fresh.txt", "T1:\\Users\\moved-notes.txt"]
            .iter().map(|s| s.to_string()).collect();
        want.sort();
        assert_eq!(all(&e), want);
    }

    #[test]
    fn wrapped_journal_forces_a_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("T1:");
        start(&[&fake], cfg(Some(dir.path()))).save(SavePolicy::default());
        fake.create(20, 11, "while-away.txt", false);
        fake.wrap_journal(); // the record above is gone
        let e = start(&[&fake], cfg(Some(dir.path())));
        assert_eq!(fake.scans(), 2);
        assert_eq!(find(&e, "while-away"), ["T1:\\Users\\Zach\\while-away.txt"]);
    }

    #[test]
    fn reset_journal_forces_a_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("T1:");
        start(&[&fake], cfg(Some(dir.path()))).save(SavePolicy::default());
        fake.create(20, 11, "x.txt", false);
        fake.reset_journal();
        let e = start(&[&fake], cfg(Some(dir.path())));
        assert_eq!(fake.scans(), 2);
        assert_eq!(find(&e, "x.txt").len(), 1);
    }

    #[test]
    fn corrupt_snapshot_forces_a_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("T1:");
        start(&[&fake], cfg(Some(dir.path()))).save(SavePolicy::default());
        let p = snapshot::path_for(dir.path(), "T1");
        let mut bytes = std::fs::read(&p).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        std::fs::write(&p, bytes).unwrap();
        let e = start(&[&fake], cfg(Some(dir.path())));
        assert_eq!(fake.scans(), 2);
        assert_eq!(find(&e, "notes").len(), 1);
    }

    #[test]
    fn force_rescan_ignores_a_good_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("T1:");
        start(&[&fake], cfg(Some(dir.path()))).save(SavePolicy::default());
        let mut c = cfg(Some(dir.path()));
        c.force_rescan = true;
        start(&[&fake], c);
        assert_eq!(fake.scans(), 2);
    }

    #[test]
    fn changes_racing_the_scan_are_not_lost_or_doubled() {
        let fake = Fake::new("T1:");
        {
            let mut fs = fake.fs.lock().unwrap();
            // After the journal position is taken: a file appears, one is
            // renamed, one is deleted. The scan already sees all of it.
            fs.during_scan = vec![
                (30, Some((11, "raced.txt".into(), false))),
                (12, Some((11, "renamed-in-scan.txt".into(), false))),
                (11, Some((10, "Zach2".into(), true))),
            ];
        }
        let e = start(&[&fake], cfg(None));
        assert_eq!(find(&e, "raced"), ["T1:\\Users\\Zach2\\raced.txt"]);
        assert_eq!(find(&e, "renamed-in"), ["T1:\\Users\\Zach2\\renamed-in-scan.txt"]);
        assert!(find(&e, "notes.txt").is_empty());
        assert_eq!(all(&e).len(), 1 + 4); // root + Users, Zach2, raced, renamed
    }

    #[test]
    fn deletion_replayed_over_a_scan_that_already_missed_the_file() {
        let fake = Fake::new("T1:");
        {
            let mut fs = fake.fs.lock().unwrap();
            fs.during_scan = vec![(12, None)];
        }
        let e = start(&[&fake], cfg(None));
        assert!(find(&e, "notes").is_empty());
    }

    #[test]
    fn tail_thread_applies_live_changes_quickly() {
        let fake = Fake::new("T1:");
        let mut c = cfg(None);
        c.tail = true;
        let mut e = start(&[&fake], c);
        assert!(e.volumes()[0].live);

        let wait = |e: &Engine, q: &str, want: usize| -> bool {
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(2) {
                if find(e, q).len() == want {
                    return true;
                }
                thread::sleep(Duration::from_millis(2));
            }
            false
        };

        fake.create(40, 11, "livefile.txt", false);
        assert!(wait(&e, "livefile", 1), "create not seen");
        fake.rename(40, 11, "livefile-renamed.txt");
        assert!(wait(&e, "renamed", 1), "rename not seen");
        assert!(wait(&e, "livefile.txt", 0), "old name still visible");
        fake.create(41, 11, "Docs", true);
        fake.create(42, 41, "inner.txt", false);
        assert!(wait(&e, "inner", 1));
        fake.rename(41, 10, "Papers");
        let t = Instant::now();
        while find(&e, "inner") != ["T1:\\Users\\Papers\\inner.txt"] && t.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(find(&e, "inner"), ["T1:\\Users\\Papers\\inner.txt"]);
        fake.delete(40);
        assert!(wait(&e, "renamed", 0), "delete not seen");
        assert!(e.volumes()[0].applied >= 6);
        e.stop();
        assert!(!e.volumes()[0].live);
    }

    /// A snapshot write (a long read) while the tail has a change to apply
    /// (a short write) used to queue the writer behind the reader and hold
    /// every new query back until the snapshot finished.
    #[test]
    fn queries_never_wait_on_a_snapshot_write() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("T1:");
        let mut c = cfg(Some(dir.path()));
        c.tail = true;
        c.autosave = None;
        let mut e = start(&[&fake], c);
        *e.vols[0].slow_save.lock().unwrap() = Duration::from_millis(800);

        let saver = {
            let v = e.vols[0].clone();
            thread::spawn(move || v.save().unwrap())
        };
        thread::sleep(Duration::from_millis(100)); // the save is now in its slow part
        fake.create(40, 11, "during-save.txt", false);
        thread::sleep(Duration::from_millis(100)); // the tail has hit the gate

        let mut worst = Duration::ZERO;
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(400) {
            let q = Instant::now();
            assert_eq!(find(&e, "notes").len(), 1);
            worst = worst.max(q.elapsed());
            thread::sleep(Duration::from_millis(2));
        }
        assert!(worst < Duration::from_millis(100), "a query waited {worst:?} on the snapshot");
        // The change waits for the save, then lands.
        saver.join().unwrap();
        let t = Instant::now();
        while find(&e, "during-save").is_empty() && t.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(find(&e, "during-save").len(), 1);
        e.stop();
    }

    #[test]
    fn tail_rescans_when_the_journal_wraps_while_running() {
        let fake = Fake::new("T1:");
        let mut c = cfg(None);
        c.tail = true;
        let e = start(&[&fake], c);
        fake.create(50, 11, "lost-in-wrap.txt", false);
        fake.wrap_journal(); // the tail may or may not have read it already
        let t = Instant::now();
        while (fake.scans() < 2 || find(&e, "lost-in-wrap").len() != 1) && t.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(find(&e, "lost-in-wrap").len(), 1);
        // And it is still live afterwards.
        fake.create(51, 11, "after-rescan.txt", false);
        let t = Instant::now();
        while find(&e, "after-rescan").is_empty() && t.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(find(&e, "after-rescan").len(), 1);
    }

    #[test]
    fn no_journal_and_denied_are_reported_not_created() {
        let a = Fake::new("A:");
        a.fs.lock().unwrap().no_journal = true;
        let b = Fake::new("B:");
        b.fs.lock().unwrap().deny = true;
        let c = Fake::new("C:");
        let e = start(&[&a, &b, &c], cfg(None));
        assert_eq!(e.volumes().len(), 1);
        let kinds: Vec<_> = e.failures.iter().map(|f| (f.label.as_str(), f.kind)).collect();
        assert!(kinds.contains(&("A:", FailureKind::NoJournal)));
        assert!(kinds.contains(&("B:", FailureKind::AccessDenied)));
        assert_eq!(a.scans(), 0);
        assert_eq!(b.scans(), 0);
    }

    #[test]
    fn not_elevated_serves_the_snapshot_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("T1:");
        start(&[&fake], cfg(Some(dir.path()))).save(SavePolicy::default());
        fake.fs.lock().unwrap().deny = true;
        let mut c = cfg(Some(dir.path()));
        c.tail = true;
        let e = start(&[&fake], c);
        let v = &e.volumes()[0];
        assert!(v.stale && !v.live);
        assert_eq!(find(&e, "notes").len(), 1);
        assert_eq!(e.save(SavePolicy { force: true, ..Default::default() }), 0);
        // With no snapshot at all, denial is a failure.
        let other = Fake::new("T2:");
        other.fs.lock().unwrap().deny = true;
        let e = start(&[&other], cfg(Some(dir.path())));
        assert_eq!(e.failures.len(), 1);
    }

    #[test]
    fn two_volumes_keep_separate_roots_and_share_a_limit() {
        let a = Fake::new("A:");
        let b = Fake::new("B:");
        let e = start(&[&a, &b], cfg(None));
        let r = e.search("notes", QueryOpts::default());
        assert_eq!(r.total, 2);
        let mut p: Vec<_> = r.hits.iter().map(|h| h.path.clone()).collect();
        p.sort();
        assert_eq!(p, ["A:\\Users\\Zach\\notes.txt", "B:\\Users\\Zach\\notes.txt"]);

        let r = e.search("notes", QueryOpts { limit: 1, ..Default::default() });
        assert_eq!((r.hits.len(), r.total), (1, 2));
        let r = e.search("notes", QueryOpts { limit: 1, count_total: false, ..Default::default() });
        assert_eq!(r.hits.len(), 1);
    }

    #[test]
    fn save_policy_rations_writes() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("T1:");
        let e = start(&[&fake], cfg(Some(dir.path())));
        assert_eq!(e.save(SavePolicy::default()), 1);
        // Few changes, fresh snapshot: nothing to do.
        for _ in 0..3 {
            let v = &e.vols[0];
            v.since_save.fetch_add(10, Ordering::Relaxed);
        }
        assert_eq!(e.save(SavePolicy::default()), 0);
        assert_eq!(e.save(SavePolicy { min_changes: 30, ..Default::default() }), 1);
        assert_eq!(e.save(SavePolicy { max_age: Duration::ZERO, ..Default::default() }), 1);
        assert_eq!(e.save(SavePolicy { force: true, ..Default::default() }), 1);
    }

    #[test]
    fn static_indexes_search_but_never_tail_or_save() {
        let mut b = Builder::new();
        b.begin_volume("W:", 0);
        b.push(1, 0, "thing.txt", false);
        let e = Engine::from_indexes(vec![("W:".into(), b.build())]);
        assert_eq!(find(&e, "thing"), ["W:\\thing.txt"]);
        assert_eq!(e.save(SavePolicy { force: true, ..Default::default() }), 0);
    }
}
