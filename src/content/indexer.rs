//! The content indexer: an unelevated background process that keeps the
//! content index current.
//!
//! ```text
//!   helper (searchd, elevated)               indexer (searchi, as the user)
//!   ──────────────────────────               ─────────────────────────────────────────────
//!   file list  ──── ListFiles ────────────►  crawler ──► bounded queue ─┐
//!   change log ──── Events (long poll) ───►  event thread ─► debounce ───┤
//!                                                                        ▼
//!                                            dispatchers ──► extractor child processes
//!                                                 │            (parse the files, low priority,
//!                                                 ▼             killed on timeout or crash)
//!                                            writer thread ──► tantivy index (+ manifest)
//! ```
//!
//! Nothing here runs with elevated rights and nothing from this process is
//! written by the helper. Crawl work pauses while the user is active or the
//! PC is on battery; live updates (a file just saved) keep trickling through.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustc_hash::FxHashSet;

use super::config::{ContentConfig, DOCUMENT_EXTENSIONS, PathRules};
use super::extract::Status;
use super::status::{self, Progress};
use super::store::{self, FileDoc, ManEntry, Manifest, fid_of};
use super::sys;
use super::worker::{Extractor, Outcome};
use crate::events::{EventKind, Filter, PollState};
use crate::ipc::{ListCursor, Reply, Request};
use crate::pipe::Client;

/// How long after its last change a file waits before it is read, so a file
/// being saved in pieces is read once, when it is whole.
const EVENT_DEBOUNCE: Duration = Duration::from_millis(1500);
/// An extractor child idle this long is stopped to give its memory back.
const WORKER_IDLE: Duration = Duration::from_secs(300);

pub struct Options {
    /// The content directory (`%LOCALAPPDATA%\Search\content`).
    pub dir: PathBuf,
    pub cfg: ContentConfig,
    /// Executable to start as `--worker`.
    pub worker_exe: PathBuf,
    /// The helper's pipe. `None`: no helper; crawl `walk_roots` instead.
    pub pipe_name: Option<String>,
    pub walk_roots: Vec<PathBuf>,
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
    /// Honour pause-when-active and pause-on-battery.
    pub gate: bool,
    /// Run a full reconcile this often.
    pub reconcile_every: Duration,
    pub commit_every: Duration,
}

impl Options {
    pub fn new(dir: PathBuf, cfg: ContentConfig, worker_exe: PathBuf) -> Options {
        Options {
            dir,
            cfg,
            worker_exe,
            pipe_name: None,
            walk_roots: Vec::new(),
            log: Arc::new(|_| {}),
            gate: true,
            reconcile_every: Duration::from_secs(6 * 3600),
            commit_every: Duration::from_secs(10),
        }
    }
}

// ------------------------------------------------------------------ plumbing

/// A bounded FIFO: `push` waits while it is full, `pop` waits while empty.
struct Queue<T> {
    items: Mutex<VecDeque<T>>,
    not_empty: Condvar,
    not_full: Condvar,
    cap: usize,
}

impl<T> Queue<T> {
    fn new(cap: usize) -> Queue<T> {
        Queue { items: Mutex::new(VecDeque::new()), not_empty: Condvar::new(), not_full: Condvar::new(), cap }
    }

    fn push(&self, t: T, stop: &AtomicBool) -> bool {
        let mut g = self.items.lock().unwrap_or_else(|e| e.into_inner());
        while g.len() >= self.cap {
            if stop.load(Ordering::Acquire) {
                return false;
            }
            g = self.not_full.wait_timeout(g, Duration::from_millis(250)).unwrap_or_else(|e| e.into_inner()).0;
        }
        g.push_back(t);
        drop(g);
        self.not_empty.notify_one();
        true
    }

    fn pop(&self, wait: Duration) -> Option<T> {
        let mut g = self.items.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_empty() {
            g = self.not_empty.wait_timeout(g, wait).unwrap_or_else(|e| e.into_inner()).0;
        }
        let t = g.pop_front();
        drop(g);
        if t.is_some() {
            self.not_full.notify_one();
        }
        t
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.items.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// Files changed on disk, waiting out the debounce. Entries come back in the
/// order their wait ends.
#[derive(Default)]
struct Pending {
    /// lower-cased path -> when it is due.
    due: HashMap<String, Instant>,
    order: VecDeque<(Instant, String, String)>,
}

impl Pending {
    fn touch(&mut self, path: &str, after: Duration) {
        let key = path.to_lowercase();
        let at = Instant::now() + after;
        self.due.insert(key.clone(), at);
        self.order.push_back((at, key, path.to_string()));
    }

    fn forget(&mut self, path: &str) {
        self.due.remove(&path.to_lowercase());
    }

    /// The next path whose wait is over.
    fn take_due(&mut self, now: Instant) -> Option<String> {
        while let Some((at, key, _)) = self.order.front() {
            let (at, key) = (*at, key.clone());
            // An entry whose time no longer matches was touched again later
            // (or forgotten): it is stale.
            if self.due.get(&key) == Some(&at) {
                if at > now {
                    return None;
                }
                let (_, key, path) = self.order.pop_front().unwrap();
                self.due.remove(&key);
                return Some(path);
            }
            self.order.pop_front();
        }
        None
    }

    fn len(&self) -> usize {
        self.due.len()
    }
}

/// Decides when crawling may run.
struct Gate {
    enabled: bool,
    pause_when_active: bool,
    idle_secs: u64,
    pause_on_battery: bool,
    cache: Mutex<(Option<Instant>, &'static str)>,
}

impl Gate {
    fn reason(&self) -> &'static str {
        if !self.enabled {
            return "";
        }
        let mut c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if c.0.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) {
            return c.1;
        }
        let r = if self.pause_on_battery && sys::on_battery() {
            "on battery"
        } else if self.pause_when_active && sys::idle_seconds() < self.idle_secs {
            "you are active"
        } else {
            ""
        };
        *c = (Some(Instant::now()), r);
        r
    }
}

enum Msg {
    Put(FileDoc),
    Remove(String),
    RemoveFid(u64),
    RemoveFolder(String),
    /// Crawl this folder, but only after everything queued before this message
    /// (a folder removal in particular) is committed and the manifest reloaded:
    /// otherwise the crawl would trust manifest entries for files just removed
    /// and skip them (a folder renamed away and straight back).
    WalkAfterCommit(String),
    /// Commit now and reply when done.
    Barrier(SyncSender<()>),
}

#[derive(Default)]
struct Counts {
    indexed: AtomicU64,
    skipped: AtomicU64,
    failed: AtomicU64,
}

impl Counts {
    fn add(&self, s: Status, delta: i64) {
        let c = match s {
            Status::Indexed | Status::Partial => &self.indexed,
            Status::Failed | Status::Timeout => &self.failed,
            _ => &self.skipped,
        };
        if delta >= 0 {
            c.fetch_add(delta as u64, Ordering::Relaxed);
        } else {
            let _ = c.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| Some(v.saturating_sub((-delta) as u64)));
        }
    }
}

struct Shared {
    opts: Options,
    rules: PathRules,
    doc_filter: Filter,
    stop: AtomicBool,
    started_ms: u64,
    manifest: Mutex<Manifest>,
    counts: Counts,
    queued: AtomicU64,
    done: AtomicU64,
    in_flight: AtomicU64,
    listing: AtomicBool,
    helper_up: AtomicBool,
    crawl_secs: AtomicU64,
    passages: AtomicU64,
    paused: Mutex<&'static str>,
    note: Mutex<String>,
    pending: Mutex<Pending>,
    queue: Queue<String>,
    writer_tx: SyncSender<Msg>,
    gate: Gate,
    job: Option<Arc<sys::Job>>,
    wake: (Mutex<Wake>, Condvar),
    /// When the last full comparison of disk and index finished.
    last_reconcile_done: Mutex<Option<Instant>>,
}

#[derive(Default)]
struct Wake {
    reconcile: bool,
    walks: Vec<String>,
}

impl Shared {
    fn log(&self, s: &str) {
        (self.opts.log)(s);
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn set_note(&self, s: impl Into<String>) {
        *self.note.lock().unwrap_or_else(|e| e.into_inner()) = s.into();
    }

    fn request_reconcile(&self) {
        let (m, cv) = &self.wake;
        m.lock().unwrap_or_else(|e| e.into_inner()).reconcile = true;
        cv.notify_all();
    }

    fn request_walk(&self, dir: String) {
        let (m, cv) = &self.wake;
        m.lock().unwrap_or_else(|e| e.into_inner()).walks.push(dir);
        cv.notify_all();
    }

    fn send(&self, m: Msg) -> bool {
        self.writer_tx.send(m).is_ok()
    }

    fn manifest_entry(&self, fid: u64) -> Option<ManEntry> {
        self.manifest.lock().unwrap_or_else(|e| e.into_inner()).get(&fid).copied()
    }

    /// Set or clear a manifest entry, keeping the counters in step.
    fn manifest_set(&self, fid: u64, new: Option<ManEntry>) {
        let mut m = self.manifest.lock().unwrap_or_else(|e| e.into_inner());
        let old = match new {
            Some(n) => m.insert(fid, n),
            None => m.remove(&fid),
        };
        if let Some(o) = old {
            self.counts.add(o.status, -1);
        }
        if let Some(n) = new {
            self.counts.add(n.status, 1);
        }
    }

    fn manifest_replace(&self, fresh: Manifest) {
        let mut m = self.manifest.lock().unwrap_or_else(|e| e.into_inner());
        for c in [&self.counts.indexed, &self.counts.skipped, &self.counts.failed] {
            c.store(0, Ordering::Relaxed);
        }
        for e in fresh.values() {
            self.counts.add(e.status, 1);
        }
        *m = fresh;
    }

    /// Block while the gate says wait. `false`: asked to stop.
    fn wait_gate(&self) -> bool {
        loop {
            if self.stopping() {
                return false;
            }
            let r = self.gate.reason();
            *self.paused.lock().unwrap_or_else(|e| e.into_inner()) = r;
            if r.is_empty() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    fn progress(&self) -> Progress {
        // Read under the pending lock: a file leaves `pending` and becomes
        // `in_flight` inside one critical section, so it is never in neither.
        let (pending, in_flight) = {
            let g = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            (g.len() as u64, self.in_flight.load(Ordering::SeqCst))
        };
        let done = self.done.load(Ordering::SeqCst);
        let queued = self.queued.load(Ordering::SeqCst);
        let remaining_work = queued.saturating_sub(done) + in_flight;
        let listing = self.listing.load(Ordering::Relaxed);
        let waiting = self.opts.pipe_name.is_some() && !self.helper_up.load(Ordering::Relaxed);
        let reconcile_due = self.last_reconcile_done.lock().unwrap_or_else(|e| e.into_inner()).is_none();
        let phase = if waiting {
            "waiting for the helper"
        } else if listing {
            "listing"
        } else if remaining_work > 0 || pending > 0 {
            "indexing"
        } else if reconcile_due {
            "starting"
        } else {
            "idle"
        };
        let paused = *self.paused.lock().unwrap_or_else(|e| e.into_inner());
        Progress {
            phase: phase.into(),
            paused: if remaining_work > 0 || listing { paused.into() } else { String::new() },
            indexed: self.counts.indexed.load(Ordering::Relaxed),
            skipped: self.counts.skipped.load(Ordering::Relaxed),
            failed: self.counts.failed.load(Ordering::Relaxed),
            queued,
            done,
            pending_events: pending,
            passages: self.passages.load(Ordering::Relaxed),
            index_bytes: store::dir_bytes(&store::index_files_dir(&self.opts.dir)),
            crawl_secs: self.crawl_secs.load(Ordering::Relaxed),
            updated_ms: status::now_ms(),
            started_ms: self.started_ms,
            pid: std::process::id(),
            note: self.note.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        }
    }
}

fn ticks(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH).map(|d| (d.as_nanos() / 100) as u64).unwrap_or(0)
}

/// Is what the manifest remembers still right for the file as it is now?
fn unchanged(old: &ManEntry, size: u64, mtime: u64, max_bytes: u64) -> bool {
    old.size == size
        && old.mtime == mtime
        && match old.status {
            // Could not be read last time: try again.
            Status::Unreadable => false,
            // Too big then; fine to skip unless the limit was raised.
            Status::TooBig => size > max_bytes,
            _ => true,
        }
}

// ----------------------------------------------------------------- processing

enum Looked {
    Gone,
    Skip,
    File { size: u64, mtime: u64 },
}

fn look(path: &str) -> Looked {
    use std::os::windows::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => {
            if sys::is_cloud_placeholder(m.file_attributes()) {
                return Looked::Skip;
            }
            Looked::File { size: m.len(), mtime: m.modified().map(ticks).unwrap_or(0) }
        }
        Ok(_) => Looked::Skip,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound) => Looked::Gone,
        Err(_) => Looked::Skip,
    }
}

impl Shared {
    /// Read one file (if it changed) and queue its text for the index.
    fn process(&self, ex: &mut Extractor, path: &str) {
        let (size, mtime) = match look(path) {
            Looked::Gone => {
                if self.manifest_entry(fid_of(path)).is_some() {
                    self.send(Msg::Remove(path.to_string()));
                }
                return;
            }
            Looked::Skip => return,
            Looked::File { size, mtime } => (size, mtime),
        };
        let max = self.opts.cfg.max_file_bytes();
        let fid = fid_of(path);
        if let Some(old) = self.manifest_entry(fid)
            && unchanged(&old, size, mtime, max)
        {
            return;
        }
        let outcome = if size > max {
            Outcome::Skipped(super::extract::Skip::TooBig)
        } else {
            ex.extract(
                path,
                max,
                self.opts.cfg.chunk_bytes as u32,
                Duration::from_secs(self.opts.cfg.file_timeout_secs),
            )
        };
        match &outcome {
            Outcome::TimedOut => self.log(&format!("timed out, skipped: {path}")),
            Outcome::Crashed => self.log(&format!("extractor crashed on: {path}")),
            Outcome::Skipped(super::extract::Skip::Failed(m)) => self.log(&format!("could not read {path}: {m}")),
            _ => {}
        }
        let status = outcome.status();
        let chunks = match outcome {
            Outcome::Text { chunks, .. } => chunks,
            _ => Vec::new(),
        };
        self.send(Msg::Put(FileDoc { path: path.to_string(), size, mtime, status, chunks }));
    }

    fn dispatch_loop(self: &Arc<Self>) {
        sys::enter_background_mode();
        let mut ex = Extractor::new(self.opts.worker_exe.clone(), self.job.clone());
        let mut last_work = Instant::now();
        while !self.stopping() {
            // Live updates first: they are few and the user is waiting for them.
            let due = {
                let mut g = self.pending.lock().unwrap_or_else(|e| e.into_inner());
                let d = g.take_due(Instant::now());
                if d.is_some() {
                    self.in_flight.fetch_add(1, Ordering::SeqCst);
                }
                d
            };
            if let Some(path) = due {
                if self.rules.allows_file(&path) {
                    self.process(&mut ex, &path);
                }
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                last_work = Instant::now();
                continue;
            }
            match self.queue.pop(Duration::from_millis(250)) {
                Some(path) => {
                    if self.wait_gate() {
                        self.process(&mut ex, &path);
                    }
                    self.done.fetch_add(1, Ordering::SeqCst);
                    last_work = Instant::now();
                }
                None => {
                    if ex.running() && last_work.elapsed() > WORKER_IDLE {
                        ex.kill();
                    }
                }
            }
        }
    }
}

// --------------------------------------------------------------------- writer

fn writer_loop(sh: Arc<Shared>, first: store::Writer, rx: Receiver<Msg>) {
    let mut w = first;
    let mut last_commit = Instant::now();
    let mut reload_manifest = false;
    let mut failures = 0u32;
    // A file you just saved should be searchable within a few seconds: commit
    // as soon as the stream of documents pauses, and at least every `commit_every`.
    let mut last_msg = Instant::now();
    let reader = w.index().reader_builder().reload_policy(tantivy::ReloadPolicy::Manual).try_into().ok();
    let passages = |reader: &Option<tantivy::IndexReader>| {
        if let Some(r) = reader {
            let _ = r.reload();
            sh.passages.store(r.searcher().num_docs(), Ordering::Relaxed);
        }
    };
    passages(&reader);
    let mut acks: Vec<SyncSender<()>> = Vec::new();
    let mut walks: Vec<String> = Vec::new();
    loop {
        let got = rx.recv_timeout(Duration::from_millis(500));
        if got.is_ok() {
            last_msg = Instant::now();
        }
        match got {
            Ok(Msg::Put(d)) => {
                let fid = fid_of(&d.path);
                // The file may have been deleted while it was being read; do not bring it back.
                if !Path::new(&d.path).exists() {
                    w.remove_fid(fid);
                    sh.manifest_set(fid, None);
                    continue;
                }
                let entry = ManEntry { size: d.size, mtime: d.mtime, status: d.status };
                if let Err(e) = w.put(d) {
                    sh.log(&format!("index write failed: {e}"));
                    sh.set_note(format!("index write failed: {e}"));
                    continue;
                }
                sh.manifest_set(fid, Some(entry));
            }
            Ok(Msg::Remove(p)) => {
                w.remove(&p);
                sh.manifest_set(fid_of(&p), None);
            }
            Ok(Msg::RemoveFid(f)) => {
                w.remove_fid(f);
                sh.manifest_set(f, None);
            }
            Ok(Msg::RemoveFolder(d)) => {
                if let Err(e) = w.remove_folder(&d) {
                    sh.log(&format!("folder removal failed: {e}"));
                }
                reload_manifest = true;
            }
            Ok(Msg::WalkAfterCommit(d)) => walks.push(d),
            Ok(Msg::Barrier(ack)) => acks.push(ack),
            Err(RecvTimeoutError::Timeout) => {
                if sh.stopping() {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let due = w.pending > 0
            && (w.pending >= 20_000
                || last_commit.elapsed() >= sh.opts.commit_every
                || last_msg.elapsed() >= Duration::from_millis(1500)
                || !acks.is_empty()
                || !walks.is_empty());
        if !due && !((!acks.is_empty() || !walks.is_empty()) && w.pending == 0) {
            continue;
        }
        match w.commit() {
            Ok(()) => {
                failures = 0;
                if sh.note.lock().unwrap_or_else(|e| e.into_inner()).starts_with("commit failed") {
                    sh.set_note("");
                }
            }
            Err(e) => {
                failures += 1;
                sh.log(&format!("commit failed: {e}"));
                sh.set_note(format!("commit failed: {e}"));
                // Still failing (disk full? files locked?): drop what is uncommitted
                // instead of holding it in memory forever, and start the writer fresh.
                // The crawl then finds whatever was lost.
                if failures >= 3 {
                    sh.log("commit keeps failing; restarting the index writer");
                    drop(w);
                    std::thread::sleep(Duration::from_secs(5));
                    match store::Writer::open(&sh.opts.dir, 64 << 20) {
                        Ok((fresh, _)) => {
                            w = fresh;
                            reload_manifest = true;
                            failures = 0;
                            sh.request_reconcile();
                        }
                        Err(e) => {
                            sh.log(&format!("cannot reopen the index: {e}"));
                            sh.stop.store(true, Ordering::Release);
                            return;
                        }
                    }
                }
            }
        }
        last_commit = Instant::now();
        if reload_manifest {
            reload_manifest = false;
            match store::load_manifest(w.index(), w.fields()) {
                Ok(m) => sh.manifest_replace(m),
                Err(e) => sh.log(&format!("manifest reload failed: {e}")),
            }
        }
        passages(&reader);
        for a in acks.drain(..) {
            let _ = a.send(());
        }
        for d in walks.drain(..) {
            sh.request_walk(d);
        }
    }
    // Commit what is left. Merges still running are abandoned (the index stays
    // valid) rather than waited for, so quitting never takes minutes.
    let _ = w.commit();
}

// -------------------------------------------------------------------- crawler

fn is_doc_ext(path: &str) -> bool {
    path.rsplit('.')
        .next()
        .map(|e| DOCUMENT_EXTENSIONS.iter().any(|d| d.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

impl Shared {
    /// Look at one candidate from a listing and queue it if it needs reading.
    fn consider(&self, path: String, seen: &mut FxHashSet<u64>) -> bool {
        if !self.rules.allows_file(&path) {
            return true;
        }
        let fid = fid_of(&path);
        seen.insert(fid);
        let need = match look(&path) {
            Looked::File { size, mtime } => match self.manifest_entry(fid) {
                Some(old) => !unchanged(&old, size, mtime, self.opts.cfg.max_file_bytes()),
                None => true,
            },
            _ => false,
        };
        if need {
            self.queued.fetch_add(1, Ordering::Relaxed);
            return self.queue.push(path, &self.stop);
        }
        true
    }

    fn walk(&self, root: &Path, seen: &mut FxHashSet<u64>) {
        use std::os::windows::fs::MetadataExt;
        let mut stack = vec![root.to_path_buf()];
        let mut n = 0u64;
        while let Some(dir) = stack.pop() {
            if self.stopping() {
                return;
            }
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let Ok(md) = e.metadata() else { continue };
                if sys::is_reparse_point(md.file_attributes()) {
                    continue;
                }
                let p = e.path();
                let ps = p.to_string_lossy().into_owned();
                if md.is_dir() {
                    if self.rules.allows_dir(&ps) {
                        stack.push(p);
                    }
                } else {
                    n += 1;
                    if n.is_multiple_of(500) && !self.wait_gate() {
                        return;
                    }
                    if !self.consider(ps, seen) {
                        return;
                    }
                }
            }
        }
    }

    /// List through the helper: documents first, then everything else.
    fn list_via_helper(&self, name: &str, seen: &mut FxHashSet<u64>) -> Result<(), String> {
        let mut client = Client::connect(name, Duration::from_secs(5)).map_err(|e| format!("helper: {e}"))?;
        for (pass, filter) in [&self.doc_filter, &self.rules.filter].into_iter().enumerate() {
            let mut cursor = ListCursor::default();
            let mut restarts = 0;
            loop {
                if !self.wait_gate() {
                    return Ok(());
                }
                let reply = client
                    .request(&Request::ListFiles { cursor, filter: filter.clone() })
                    .map_err(|e| format!("helper: {e}"))?;
                match reply {
                    Reply::Files { restart: true, .. } => {
                        restarts += 1;
                        if restarts > 3 {
                            return Err("the helper's index keeps changing; will retry".into());
                        }
                        cursor = ListCursor::default();
                    }
                    Reply::Files { next, paths, .. } => {
                        for p in paths {
                            if pass == 1 && is_doc_ext(&p) {
                                continue; // listed in the first pass
                            }
                            if !self.consider(p, seen) {
                                return Ok(());
                            }
                        }
                        match next {
                            Some(c) => cursor = c,
                            None => break,
                        }
                    }
                    Reply::Loading(_) => std::thread::sleep(Duration::from_secs(2)),
                    other => return Err(format!("unexpected reply from the helper: {other:?}")),
                }
            }
        }
        Ok(())
    }

    /// Compare disk and index: read what is new or changed, forget what is gone.
    fn reconcile(&self) -> Result<(), String> {
        let t = Instant::now();
        self.listing.store(true, Ordering::Release);
        let before: Vec<u64> = self.manifest.lock().unwrap_or_else(|e| e.into_inner()).keys().copied().collect();
        let mut seen: FxHashSet<u64> = FxHashSet::default();
        let r = match &self.opts.pipe_name {
            Some(name) => self.list_via_helper(name, &mut seen),
            None => {
                for root in self.opts.walk_roots.clone() {
                    self.walk(&root, &mut seen);
                }
                Ok(())
            }
        };
        self.listing.store(false, Ordering::Release);
        self.crawl_secs.fetch_add(t.elapsed().as_secs(), Ordering::Relaxed);
        r?;
        if self.stopping() {
            return Ok(());
        }
        // Whatever was in the index before this pass and was not listed is gone
        // (deleted, moved, or no longer allowed by the settings). But a listing
        // that is suddenly missing most of what we know (a drive the helper failed
        // to load, a settings change) is more likely wrong than a mass deletion:
        // deletions arrive as events anyway, so keep everything and say so.
        let missing: Vec<u64> = before.iter().copied().filter(|f| !seen.contains(f)).collect();
        let mut gone = 0;
        if missing.len() > 5000 && missing.len() * 4 > before.len() {
            let msg = format!(
                "the listing lacks {} of the {} files in the index; keeping them (if you changed the settings, run searchi --rebuild)",
                missing.len(),
                before.len()
            );
            self.log(&msg);
            self.set_note(msg);
        } else {
            for fid in missing {
                gone += 1;
                self.send(Msg::RemoveFid(fid));
            }
        }
        self.log(&format!(
            "reconcile: {} files seen, {} to read, {gone} removed, listing took {:.1}s",
            seen.len(),
            self.queued.load(Ordering::Relaxed),
            t.elapsed().as_secs_f64()
        ));
        Ok(())
    }

    fn crawl_loop(self: &Arc<Self>) {
        sys::enter_background_mode();
        loop {
            let (what_walks, do_reconcile) = {
                let (m, cv) = &self.wake;
                let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
                while !self.stopping() && !g.reconcile && g.walks.is_empty() {
                    let wait = self
                        .last_reconcile_done
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .map_or(Duration::from_secs(1), |t| self.opts.reconcile_every.saturating_sub(t.elapsed()));
                    if wait.is_zero() {
                        g.reconcile = true;
                        break;
                    }
                    g = cv.wait_timeout(g, wait.min(Duration::from_secs(5))).unwrap_or_else(|e| e.into_inner()).0;
                }
                (std::mem::take(&mut g.walks), std::mem::take(&mut g.reconcile))
            };
            if self.stopping() {
                return;
            }
            if do_reconcile {
                match self.reconcile() {
                    Ok(()) => {
                        *self.last_reconcile_done.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                    }
                    Err(e) => {
                        self.log(&format!("reconcile failed: {e}"));
                        self.set_note(e);
                        // Try again soon.
                        for _ in 0..30 {
                            if self.stopping() {
                                return;
                            }
                            std::thread::sleep(Duration::from_secs(1));
                        }
                        self.request_reconcile();
                    }
                }
            }
            for dir in what_walks {
                let mut seen = FxHashSet::default();
                self.walk(Path::new(&dir), &mut seen);
            }
        }
    }

    // --------------------------------------------------------- live updates

    fn handle_events(&self, events: Vec<crate::events::Event>) {
        for e in events {
            match (e.kind, e.is_dir) {
                (EventKind::Changed, false) => {
                    if self.rules.allows_file(&e.path) {
                        self.pending.lock().unwrap_or_else(|x| x.into_inner()).touch(&e.path, EVENT_DEBOUNCE);
                    }
                }
                (EventKind::Deleted, false) => {
                    self.pending.lock().unwrap_or_else(|x| x.into_inner()).forget(&e.path);
                    self.send(Msg::Remove(e.path));
                }
                (EventKind::Deleted, true) => {
                    self.send(Msg::RemoveFolder(e.path));
                }
                (EventKind::Renamed, false) => {
                    self.pending.lock().unwrap_or_else(|x| x.into_inner()).forget(&e.old_path);
                    self.send(Msg::Remove(e.old_path));
                    if self.rules.allows_file(&e.path) {
                        self.pending.lock().unwrap_or_else(|x| x.into_inner()).touch(&e.path, EVENT_DEBOUNCE);
                    }
                }
                (EventKind::Renamed, true) => {
                    self.send(Msg::RemoveFolder(e.old_path));
                    if self.rules.allows_dir(&e.path) {
                        self.send(Msg::WalkAfterCommit(e.path));
                    }
                }
                (EventKind::Changed, true) => {
                    if self.rules.allows_dir(&e.path) {
                        self.request_walk(e.path);
                    }
                }
            }
        }
    }

    fn event_loop(self: &Arc<Self>, name: String) {
        let mut client: Option<Client> = None;
        let mut sub: Option<(u64, u64)> = None;
        while !self.stopping() {
            let c = match client.as_mut() {
                Some(c) => c,
                None => match Client::connect(&name, Duration::from_millis(500)) {
                    Ok(c) => {
                        sub = None;
                        client = Some(c);
                        client.as_mut().unwrap()
                    }
                    Err(_) => {
                        self.helper_up.store(false, Ordering::Relaxed);
                        sleep_unless_stopped(self, Duration::from_secs(3));
                        continue;
                    }
                },
            };
            if sub.is_none() {
                match c.request(&Request::Subscribe(self.rules.filter.clone())) {
                    Ok(Reply::Subscribed { epoch, next }) => {
                        sub = Some((epoch, next));
                        self.helper_up.store(true, Ordering::Relaxed);
                        self.log("subscribed to the helper's change events");
                        // Compare disk and index again: events before this point were not recorded.
                        self.request_reconcile();
                    }
                    Ok(Reply::Loading(_)) => {
                        self.helper_up.store(true, Ordering::Relaxed);
                        sleep_unless_stopped(self, Duration::from_secs(2));
                    }
                    Ok(other) => {
                        self.log(&format!("helper refused the subscription: {other:?}"));
                        self.set_note(match other {
                            Reply::Error(e) => format!("the helper refused the settings: {e}"),
                            _ => "the helper is an older version; run install.ps1".to_string(),
                        });
                        client = None;
                        sleep_unless_stopped(self, Duration::from_secs(10));
                    }
                    Err(_) => {
                        client = None;
                        self.helper_up.store(false, Ordering::Relaxed);
                    }
                }
                continue;
            }
            let (epoch, next) = sub.unwrap();
            match c.request(&Request::Events { epoch, next, max: 1000, wait_ms: 3000 }) {
                Ok(Reply::Events { state, next, events, .. }) => match state {
                    PollState::Ok => {
                        sub = Some((epoch, next));
                        self.handle_events(events);
                    }
                    PollState::Lost | PollState::NewEpoch | PollState::NotSubscribed => {
                        self.log(&format!("event stream reset ({state:?}); resubscribing"));
                        sub = None;
                    }
                },
                Ok(Reply::Loading(_)) => sleep_unless_stopped(self, Duration::from_secs(2)),
                Ok(_) => sub = None,
                Err(_) => {
                    client = None;
                    self.helper_up.store(false, Ordering::Relaxed);
                }
            }
        }
    }
}

fn sleep_unless_stopped(sh: &Shared, d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end && !sh.stopping() {
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------- start

pub struct Running {
    sh: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

pub fn start(opts: Options) -> Result<Running, String> {
    std::fs::create_dir_all(&opts.dir).map_err(|e| format!("cannot create {}: {e}", opts.dir.display()))?;
    let (writer_obj, fresh) = store::Writer::open(&opts.dir, 64 << 20).map_err(|e| format!("cannot open the index: {e}"))?;
    let manifest = if fresh {
        Manifest::default()
    } else {
        store::load_manifest(writer_obj.index(), writer_obj.fields()).map_err(|e| format!("cannot read the index: {e}"))?
    };

    let (mut rules, problems) = PathRules::new(&opts.cfg);
    for p in problems {
        (opts.log)(&format!("config: {p}"));
    }
    // The index itself is never indexed.
    let own = super::config::app_dir().to_string_lossy().to_lowercase();
    rules.filter.exclude_fragments.push(format!("{}\\", own.trim_end_matches('\\')));
    for p in rules.filter.clamp() {
        (opts.log)(&format!("config: {p}"));
    }
    let mut doc_filter = rules.filter.clone();
    doc_filter.exts.retain(|e| DOCUMENT_EXTENSIONS.contains(&e.as_str()));

    let (tx, rx) = sync_channel(256);
    let sh = Arc::new(Shared {
        gate: Gate {
            enabled: opts.gate,
            pause_when_active: opts.cfg.pause_when_active,
            idle_secs: opts.cfg.idle_secs,
            pause_on_battery: opts.cfg.pause_on_battery,
            cache: Mutex::new((None, "")),
        },
        rules,
        doc_filter,
        stop: AtomicBool::new(false),
        started_ms: status::now_ms(),
        manifest: Mutex::new(Manifest::default()),
        counts: Counts::default(),
        queued: AtomicU64::new(0),
        done: AtomicU64::new(0),
        in_flight: AtomicU64::new(0),
        listing: AtomicBool::new(false),
        helper_up: AtomicBool::new(false),
        crawl_secs: AtomicU64::new(0),
        passages: AtomicU64::new(0),
        paused: Mutex::new(""),
        note: Mutex::new(String::new()),
        pending: Mutex::new(Pending::default()),
        queue: Queue::new(512),
        writer_tx: tx,
        job: sys::Job::new(2 << 30).map(Arc::new),
        wake: (Mutex::new(Wake::default()), Condvar::new()),
        last_reconcile_done: Mutex::new(None),
        opts,
    });
    sh.manifest_replace(manifest);
    sh.log(&format!(
        "indexer started: {} files known, helper: {}",
        sh.manifest.lock().unwrap_or_else(|e| e.into_inner()).len(),
        sh.opts.pipe_name.as_deref().unwrap_or("none (walking)")
    ));

    let mut threads = Vec::new();
    let mut spawn = |name: &str, f: Box<dyn FnOnce() + Send>| {
        let sh2 = sh.clone();
        threads.push(
            std::thread::Builder::new()
                .name(name.into())
                .spawn(move || {
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
                        sh2.log("a thread panicked; stopping the indexer");
                        sh2.stop.store(true, Ordering::Release);
                    }
                })
                .expect("spawn thread"),
        );
    };
    {
        let sh2 = sh.clone();
        spawn("writer", Box::new(move || writer_loop(sh2.clone(), writer_obj, rx)));
    }
    for i in 0..sh.opts.cfg.workers {
        let sh2 = sh.clone();
        spawn(&format!("dispatch-{i}"), Box::new(move || sh2.dispatch_loop()));
    }
    {
        let sh2 = sh.clone();
        spawn("crawler", Box::new(move || sh2.crawl_loop()));
    }
    match sh.opts.pipe_name.clone() {
        Some(name) => {
            let sh2 = sh.clone();
            spawn("events", Box::new(move || sh2.event_loop(name)));
        }
        None => sh.request_reconcile(),
    }
    {
        let sh2 = sh.clone();
        spawn(
            "status",
            Box::new(move || {
                while !sh2.stopping() {
                    let p = sh2.progress();
                    let _ = status::write(&sh2.opts.dir, &p);
                    sleep_unless_stopped(&sh2, Duration::from_secs(2));
                }
                let mut p = sh2.progress();
                p.phase = "stopped".into();
                p.updated_ms = 0;
                let _ = status::write(&sh2.opts.dir, &p);
            }),
        );
    }
    Ok(Running { sh, threads })
}

impl Running {
    pub fn progress(&self) -> Progress {
        self.sh.progress()
    }

    pub fn stopping(&self) -> bool {
        self.sh.stopping()
    }

    /// Ask for a new comparison of disk and index.
    pub fn reconcile(&self) {
        self.sh.request_reconcile();
    }

    /// Block until everything found so far is in the committed index, or the
    /// timeout passes. For tests and the one-shot CLI mode.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if self.sh.stopping() {
                return false;
            }
            let p = self.sh.progress();
            if p.phase == "idle" {
                // Idle means the listing finished and the queues are empty; flush the writer.
                let (tx, rx) = sync_channel(1);
                if self.sh.send(Msg::Barrier(tx)) && rx.recv_timeout(Duration::from_secs(30)).is_ok() {
                    let p = self.sh.progress();
                    if p.phase == "idle" {
                        return true;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Commit everything written so far.
    pub fn flush(&self) {
        let (tx, rx) = sync_channel(1);
        if self.sh.send(Msg::Barrier(tx)) {
            let _ = rx.recv_timeout(Duration::from_secs(30));
        }
    }

    /// Stop every thread (the writer commits what it has first).
    pub fn stop(self) {
        self.sh.stop.store(true, Ordering::Release);
        self.sh.wake.1.notify_all();
        // A child busy on a slow file would hold the join below for its whole time limit.
        if let Some(job) = &self.sh.job {
            job.terminate();
        }
        for t in self.threads {
            let name = t.thread().name().unwrap_or("?").to_string();
            let _ = t.join();
            self.sh.log(&format!("stopped thread {name}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_debounces_and_orders() {
        let mut p = Pending::default();
        p.touch(r"C:\a.txt", Duration::from_millis(0));
        p.touch(r"C:\b.txt", Duration::from_millis(40));
        p.touch(r"C:\A.TXT", Duration::from_millis(60)); // same file, touched again later
        assert_eq!(p.len(), 2);
        std::thread::sleep(Duration::from_millis(10));
        // a.txt was touched again, so it is no longer due; b.txt is not due yet either.
        assert_eq!(p.take_due(Instant::now()), None);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(p.take_due(Instant::now()).as_deref(), Some(r"C:\b.txt"));
        assert_eq!(p.take_due(Instant::now()).as_deref(), Some(r"C:\A.TXT"));
        assert_eq!(p.take_due(Instant::now()), None);
        assert_eq!(p.len(), 0);
        // A forgotten path never comes back.
        p.touch(r"C:\c.txt", Duration::ZERO);
        p.forget(r"c:\C.txt");
        assert_eq!(p.take_due(Instant::now() + Duration::from_secs(1)), None);
    }

    #[test]
    fn queue_blocks_when_full_and_stops() {
        let q: Queue<u32> = Queue::new(2);
        let stop = AtomicBool::new(false);
        assert!(q.push(1, &stop) && q.push(2, &stop));
        stop.store(true, Ordering::Release);
        assert!(!q.push(3, &stop));
        assert_eq!(q.pop(Duration::from_millis(1)), Some(1));
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn unchanged_rules() {
        let e = |size, mtime, status| ManEntry { size, mtime, status };
        assert!(unchanged(&e(5, 9, Status::Indexed), 5, 9, 100));
        assert!(!unchanged(&e(5, 9, Status::Indexed), 6, 9, 100));
        assert!(!unchanged(&e(5, 9, Status::Indexed), 5, 10, 100));
        assert!(!unchanged(&e(5, 9, Status::Unreadable), 5, 9, 100)); // retried
        assert!(unchanged(&e(500, 9, Status::TooBig), 500, 9, 100));
        assert!(!unchanged(&e(500, 9, Status::TooBig), 500, 9, 1000)); // limit raised
        assert!(unchanged(&e(5, 9, Status::Timeout), 5, 9, 100)); // not retried until it changes
    }

    #[test]
    fn document_extensions_are_recognised() {
        assert!(is_doc_ext(r"C:\a\b.PDF"));
        assert!(!is_doc_ext(r"C:\a\b.rs"));
    }
}
