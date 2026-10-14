//! Content results for the popup, on a thread of their own.
//!
//! The name search goes to the helper and answers in a millisecond or two;
//! this one reads the content index directly and takes a little longer. It
//! must never hold the name results back, so it has its own thread, waits for
//! a short pause in typing before it searches, and drops every query that a
//! newer keystroke has replaced.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use search::content::search::{ContentReader, is_content_query};
use search::content::status;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::WM_APP_CONTENT;

/// Wait this long after the last keystroke before searching.
const QUIET: Duration = Duration::from_millis(90);

pub enum Job {
    /// Search for `text`. An empty text just forgets the previous query.
    Query { seq: u64, text: String },
}

pub struct ContentRow {
    pub path: String,
    pub snippet: String,
    pub highlights: Vec<Range<usize>>,
    pub page: u32,
}

pub struct Answer {
    pub seq: u64,
    pub rows: Vec<ContentRow>,
    /// The indexer is still working through the first crawl, so results may be incomplete.
    pub still_indexing: bool,
}

pub type Slot = Arc<Mutex<Option<Answer>>>;

pub struct ContentWorker {
    tx: Sender<Job>,
    /// The newest query sequence number; older ones are dropped unanswered.
    latest: Arc<AtomicU64>,
}

impl ContentWorker {
    pub fn start(hwnd: HWND, slot: Slot, dir: PathBuf, max_results: usize) -> ContentWorker {
        let (tx, rx) = channel();
        let latest = Arc::new(AtomicU64::new(0));
        let (l2, hwnd) = (latest.clone(), hwnd as usize);
        std::thread::Builder::new()
            .name("content-search".into())
            .spawn(move || run(rx, hwnd, slot, dir, max_results, l2))
            .expect("spawn content worker");
        ContentWorker { tx, latest }
    }

    pub fn send(&self, seq: u64, text: String) {
        self.latest.store(seq, Ordering::Release);
        let _ = self.tx.send(Job::Query { seq, text });
    }
}

fn run(rx: Receiver<Job>, hwnd: usize, slot: Slot, dir: PathBuf, max_results: usize, latest: Arc<AtomicU64>) {
    let mut reader: Option<ContentReader> = None;
    let mut tried = Instant::now() - Duration::from_secs(60);
    let mut progress_at = Instant::now() - Duration::from_secs(60);
    let mut indexing = false;
    loop {
        // While idle, pick up the indexer's commits now and then: a reader that
        // never reloads keeps old index files mapped, and the indexer cannot
        // delete them (it would pile up gigabytes during a long first crawl).
        let first = match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(j) => j,
            Err(RecvTimeoutError::Timeout) => {
                if let Some(r) = reader.as_mut() {
                    r.reload();
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let Job::Query { mut seq, mut text } = first;
        // Keep taking newer queries until typing pauses.
        loop {
            match rx.recv_timeout(QUIET) {
                Ok(Job::Query { seq: s, text: t }) => {
                    seq = s;
                    text = t;
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        let publish = |rows: Vec<ContentRow>, still_indexing: bool| {
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(Answer { seq, rows, still_indexing });
            // SAFETY: posting to a window handle; if the window is gone this just fails.
            unsafe { PostMessageW(hwnd as HWND, WM_APP_CONTENT, 0, 0) };
        };
        // Short text, or the name search's own syntax (wildcards, ext:, regex:, paths): no content search.
        if text.trim().chars().count() < 2 || !is_content_query(&text) {
            publish(Vec::new(), false);
            continue;
        }
        if latest.load(Ordering::Acquire) != seq {
            continue; // a newer keystroke arrived while waiting
        }
        if reader.is_none() && tried.elapsed() > Duration::from_secs(2) {
            tried = Instant::now();
            reader = ContentReader::open(&dir).ok();
        }
        if progress_at.elapsed() > Duration::from_secs(3) {
            progress_at = Instant::now();
            indexing = status::read(&dir)
                .map(|p| p.is_running(status::now_ms()) && p.phase != "idle")
                .unwrap_or(false);
        }
        let Some(r) = reader.as_mut() else {
            publish(Vec::new(), false);
            continue;
        };
        // A damaged index must not take the popup down: if the library panics, forget
        // the reader and try to open it afresh a moment later.
        let searched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if r.age() > Duration::from_secs(2) {
                r.reload();
            }
            r.search(&text, max_results)
        }));
        let Ok(hits) = searched else {
            reader = None;
            publish(Vec::new(), false);
            continue;
        };
        if latest.load(Ordering::Acquire) != seq {
            continue; // superseded while searching: do not show an old answer
        }
        let rows = hits
            .into_iter()
            .map(|h| ContentRow { path: h.path, snippet: h.snippet, highlights: h.highlights, page: h.page })
            .collect();
        publish(rows, indexing);
    }
}
