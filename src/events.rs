//! What the elevated helper tells the unelevated indexer: which files were
//! created, changed, renamed or deleted, plus the path rules both sides share.
//!
//! The helper only *records* paths here (it never opens or parses a file);
//! the indexer decides what to do with them. Nothing in this module touches
//! the OS, so it is tested directly.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Events kept for a slow subscriber; older ones are dropped and the
/// subscriber is told it lost some (it then does a full catch-up).
pub const RING_CAP: usize = 100_000;
/// Most events one reply carries.
pub const MAX_EVENTS_PER_REPLY: usize = 2_000;
/// A subscriber that has not polled for this long is forgotten, so a dead
/// indexer does not make the helper keep recording.
pub const SUBSCRIBER_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A file was created, written (and closed) or moved in.
    Changed,
    Deleted,
    /// `old_path` became `path` (a rename or a move).
    Renamed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub seq: u64,
    pub kind: EventKind,
    pub is_dir: bool,
    pub path: String,
    /// Only for `Renamed`; empty otherwise.
    pub old_path: String,
}

/// Which paths are worth hearing about. Everything is lower case; the helper
/// and the indexer apply it the same way (`allows_file` / `allows_dir`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// File extensions without the dot. A file with none of these is not wanted.
    pub exts: Vec<String>,
    /// Folder names that are never entered (`node_modules`, `.git`, ...).
    pub exclude_components: Vec<String>,
    /// Folder paths below the drive root, with leading and trailing backslash
    /// (`\windows\`), on any drive.
    pub exclude_prefixes: Vec<String>,
    /// Substrings of the whole path (`\appdata\local\`).
    pub exclude_fragments: Vec<String>,
    /// If not empty, only paths below one of these (`c:\users\zach\`).
    pub include_prefixes: Vec<String>,
    /// Never enter a folder whose name starts with a dot (`.vscode`, `.cache`,
    /// `.codex`, ...): tool and cache folders, not documents.
    pub skip_dot_dirs: bool,
}

/// Limits on a filter that arrives over the pipe. The helper is elevated and
/// applies the filter to every journal record, so its cost per record is
/// bounded: at most this many items per list (substring tests are the
/// expensive ones, hence the small number of fragments), each at most a path
/// long.
pub const MAX_FILTER_ITEMS: [usize; 5] = [256, 256, 64, 32, 64];
pub const MAX_FILTER_ITEM_BYTES: usize = 260;

impl Filter {
    /// Cut every list (and item) down to what the helper accepts. Returns the
    /// problems found, for the log.
    pub fn clamp(&mut self) -> Vec<String> {
        let mut problems = Vec::new();
        let names = ["extensions", "excluded folder names", "excluded paths", "excluded path fragments", "included paths"];
        let lists = [
            &mut self.exts,
            &mut self.exclude_components,
            &mut self.exclude_prefixes,
            &mut self.exclude_fragments,
            &mut self.include_prefixes,
        ];
        for ((list, max), name) in lists.into_iter().zip(MAX_FILTER_ITEMS).zip(names) {
            if list.len() > max {
                problems.push(format!("too many {name} ({}); only the first {max} are used", list.len()));
                list.truncate(max);
            }
            let before = list.len();
            list.retain(|s| s.len() <= MAX_FILTER_ITEM_BYTES);
            if list.len() < before {
                problems.push(format!("{} of the {name} are too long and ignored", before - list.len()));
            }
        }
        problems
    }

    fn ext_ok(&self, name: &str) -> bool {
        let Some(dot) = name.rfind('.') else {
            return self.exts.iter().any(|e| e.is_empty());
        };
        let ext = &name[dot + 1..];
        self.exts.iter().any(|e| e.eq_ignore_ascii_case(ext))
    }

    /// `lower` is the lower-cased full path. `skip_last`: the final component
    /// is a file name, not a folder.
    fn path_ok(&self, lower: &str, skip_last: bool) -> bool {
        // Below an include prefix the folder-name rules do not apply to the prefix
        // itself: you asked for that folder, whatever it is called.
        let mut below = 0;
        if !self.include_prefixes.is_empty() {
            match self.include_prefixes.iter().find(|p| lower.starts_with(p.as_str())) {
                Some(p) => below = p.len(),
                None => return false,
            }
        }
        let tail = if lower.as_bytes().get(1) == Some(&b':') { &lower[2..] } else { lower };
        if self.exclude_prefixes.iter().any(|p| tail.starts_with(p.as_str())) {
            return false;
        }
        if self.exclude_fragments.iter().any(|f| lower.contains(f.as_str())) {
            return false;
        }
        let names = if below > 0 { &lower[below.min(lower.len())..] } else { tail };
        let mut comps = names.split('\\').filter(|c| !c.is_empty()).peekable();
        while let Some(c) = comps.next() {
            if skip_last && comps.peek().is_none() {
                break;
            }
            if self.skip_dot_dirs && c.starts_with('.') {
                return false;
            }
            if self.exclude_components.iter().any(|x| x == c) {
                return false;
            }
        }
        true
    }

    pub fn allows_file(&self, path: &str) -> bool {
        let lower = path.to_lowercase();
        let name = lower.rsplit('\\').next().unwrap_or("");
        self.ext_ok(name) && self.path_ok(&lower, true)
    }

    /// For a file name alone (the helper checks this before it builds a path).
    pub fn allows_name(&self, name: &str) -> bool {
        self.ext_ok(name)
    }

    pub fn allows_dir(&self, path: &str) -> bool {
        let mut lower = path.to_lowercase();
        if !lower.ends_with('\\') {
            lower.push('\\');
        }
        self.path_ok(&lower, false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollState {
    Ok,
    /// The subscriber fell behind and events were dropped.
    Lost,
    /// The helper restarted since `subscribe`: sequence numbers mean nothing now.
    NewEpoch,
    /// Nobody is subscribed (never, or timed out).
    NotSubscribed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Polled {
    pub state: PollState,
    pub epoch: u64,
    /// Sequence number to ask for next.
    pub next: u64,
    pub events: Vec<Event>,
}

struct Inner {
    filter: Option<Arc<Filter>>,
    ring: VecDeque<Event>,
    next_seq: u64,
    last_poll: Instant,
}

pub struct EventLog {
    epoch: u64,
    inner: Mutex<Inner>,
    cv: Condvar,
}

impl Default for EventLog {
    fn default() -> Self {
        Self::new()
    }
}

impl EventLog {
    pub fn new() -> EventLog {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        EventLog {
            epoch: (t ^ ((std::process::id() as u64) << 40)) | 1,
            inner: Mutex::new(Inner {
                filter: None,
                ring: VecDeque::new(),
                next_seq: 1,
                last_poll: Instant::now(),
            }),
            cv: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Start (or restart) recording what `filter` allows. Returns the epoch and
    /// the first sequence number the subscriber will see.
    pub fn subscribe(&self, filter: Filter) -> (u64, u64) {
        let mut g = self.lock();
        g.filter = Some(Arc::new(filter));
        g.last_poll = Instant::now();
        (self.epoch, g.next_seq)
    }

    /// The rules in force, or `None` if nobody is listening (the helper then
    /// skips building paths altogether).
    pub fn filter(&self) -> Option<Arc<Filter>> {
        let mut g = self.lock();
        if g.filter.is_some() && g.last_poll.elapsed() > SUBSCRIBER_TIMEOUT {
            g.filter = None;
            g.ring.clear();
        }
        g.filter.clone()
    }

    pub fn push(&self, kind: EventKind, is_dir: bool, path: String, old_path: String) {
        let mut g = self.lock();
        if g.filter.is_none() {
            return;
        }
        let seq = g.next_seq;
        g.next_seq += 1;
        g.ring.push_back(Event { seq, kind, is_dir, path, old_path });
        while g.ring.len() > RING_CAP {
            g.ring.pop_front();
        }
        drop(g);
        self.cv.notify_all();
    }

    /// Events from `next` on. Waits up to `wait` if there are none yet.
    pub fn poll(&self, epoch: u64, next: u64, max: usize, wait: Duration) -> Polled {
        let max = max.clamp(1, MAX_EVENTS_PER_REPLY);
        let deadline = Instant::now() + wait;
        let mut g = self.lock();
        loop {
            if epoch != self.epoch {
                return Polled { state: PollState::NewEpoch, epoch: self.epoch, next: g.next_seq, events: Vec::new() };
            }
            if g.filter.is_none() {
                return Polled { state: PollState::NotSubscribed, epoch, next: g.next_seq, events: Vec::new() };
            }
            g.last_poll = Instant::now();
            let first = g.ring.front().map_or(g.next_seq, |e| e.seq);
            if next < first {
                return Polled { state: PollState::Lost, epoch, next: g.next_seq, events: Vec::new() };
            }
            if next < g.next_seq {
                let skip = (next - first) as usize;
                let events: Vec<Event> = g.ring.iter().skip(skip).take(max).cloned().collect();
                let after = events.last().map_or(next, |e| e.seq + 1);
                return Polled { state: PollState::Ok, epoch, next: after, events };
            }
            let now = Instant::now();
            if now >= deadline {
                return Polled { state: PollState::Ok, epoch, next, events: Vec::new() };
            }
            g = self.cv.wait_timeout(g, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filt() -> Filter {
        Filter {
            exts: vec!["txt".into(), "md".into(), "pdf".into()],
            exclude_components: vec!["node_modules".into(), ".git".into()],
            exclude_prefixes: vec![r"\windows\".into(), r"\program files\".into()],
            exclude_fragments: vec![r"\appdata\local\".into()],
            include_prefixes: Vec::new(),
            skip_dot_dirs: false,
        }
    }

    #[test]
    fn clamping_keeps_a_filter_within_what_the_helper_accepts() {
        let mut f = filt();
        f.exclude_fragments = (0..40).map(|i| format!("frag{i}")).collect();
        f.exts.push("x".repeat(300));
        let problems = f.clamp();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert_eq!(f.exclude_fragments.len(), MAX_FILTER_ITEMS[3]);
        assert!(f.exts.iter().all(|e| e.len() <= MAX_FILTER_ITEM_BYTES));
        assert!(filt().clamp().is_empty());
    }

    #[test]
    fn an_include_prefix_may_itself_be_a_dot_folder() {
        let mut f = filt();
        f.skip_dot_dirs = true;
        f.include_prefixes = vec![r"c:\users\zach\.notes\".into()];
        assert!(f.allows_file(r"C:\Users\Zach\.notes\a.txt"));
        assert!(f.allows_file(r"C:\Users\Zach\.notes\sub\a.txt"));
        assert!(!f.allows_file(r"C:\Users\Zach\.notes\.hidden\a.txt"));
    }

    #[test]
    fn dot_folders_can_be_skipped() {
        let mut f = filt();
        f.skip_dot_dirs = true;
        assert!(!f.allows_file(r"C:\Users\Zach\.vscode\extensions\a.txt"));
        assert!(!f.allows_dir(r"C:\Users\Zach\.codex"));
        assert!(f.allows_file(r"C:\Users\Zach\notes\a.txt"));
        // A dot-file's own name is not a folder.
        assert!(f.allows_file(r"C:\Users\Zach\.notes.txt"));
        f.skip_dot_dirs = false;
        assert!(f.allows_file(r"C:\Users\Zach\.vscode\extensions\a.txt"));
    }

    #[test]
    fn filter_applies_extension_and_exclusions() {
        let f = filt();
        assert!(f.allows_file(r"C:\Users\Zach\Notes.TXT"));
        assert!(f.allows_file(r"D:\docs\a.pdf"));
        assert!(!f.allows_file(r"C:\Users\Zach\a.exe"));
        assert!(!f.allows_file(r"C:\Users\Zach\noext"));
        assert!(!f.allows_file(r"C:\Windows\System32\a.txt"));
        assert!(!f.allows_file(r"E:\Windows\a.txt")); // any drive
        assert!(f.allows_file(r"C:\Users\windows.txt")); // only whole folders match
        assert!(!f.allows_file(r"C:\proj\node_modules\x\a.md"));
        assert!(!f.allows_file(r"C:\proj\.git\HEAD.txt"));
        assert!(!f.allows_file(r"C:\Users\Zach\AppData\Local\Temp\a.txt"));
        assert!(f.allows_file(r"C:\Users\Zach\AppData\Roaming\a.txt"));
        // The last component of a file path is a name, not a folder.
        assert!(f.allows_file(r"C:\proj\node_modules.md"));
        assert!(!f.allows_dir(r"C:\proj\node_modules"));
        assert!(f.allows_dir(r"C:\proj\src"));
        assert!(!f.allows_dir(r"C:\Windows"));
    }

    #[test]
    fn include_prefixes_restrict() {
        let mut f = filt();
        f.include_prefixes = vec![r"c:\users\zach\".into()];
        assert!(f.allows_file(r"C:\Users\Zach\a.txt"));
        assert!(!f.allows_file(r"C:\Users\Other\a.txt"));
        assert!(!f.allows_file(r"D:\a.txt"));
    }

    #[test]
    fn events_flow_in_order_and_only_after_subscribing() {
        let log = EventLog::new();
        log.push(EventKind::Changed, false, r"C:\a.txt".into(), String::new());
        assert!(log.filter().is_none());
        let (epoch, next) = log.subscribe(filt());
        assert_eq!(next, 1);
        log.push(EventKind::Changed, false, r"C:\a.txt".into(), String::new());
        log.push(EventKind::Renamed, false, r"C:\b.txt".into(), r"C:\a.txt".into());
        log.push(EventKind::Deleted, false, r"C:\b.txt".into(), String::new());
        let p = log.poll(epoch, next, 2, Duration::ZERO);
        assert_eq!(p.state, PollState::Ok);
        assert_eq!(p.events.len(), 2);
        assert_eq!(p.events[1].old_path, r"C:\a.txt");
        assert_eq!(p.next, 3);
        let p = log.poll(epoch, p.next, 10, Duration::ZERO);
        assert_eq!(p.events.len(), 1);
        assert_eq!(p.events[0].kind, EventKind::Deleted);
        assert_eq!(p.next, 4);
        // Nothing new: empty, same position.
        let p = log.poll(epoch, 4, 10, Duration::from_millis(5));
        assert!(p.events.is_empty());
        assert_eq!(p.next, 4);
    }

    #[test]
    fn wrong_epoch_and_dropped_events_are_reported() {
        let log = EventLog::new();
        let (epoch, next) = log.subscribe(filt());
        assert_eq!(log.poll(epoch ^ 2, next, 1, Duration::ZERO).state, PollState::NewEpoch);
        for i in 0..(RING_CAP + 10) {
            log.push(EventKind::Changed, false, format!(r"C:\{i}.txt"), String::new());
        }
        let p = log.poll(epoch, next, 10, Duration::ZERO);
        assert_eq!(p.state, PollState::Lost);
        // Asking from "now" works again.
        let p2 = log.poll(epoch, p.next, 10, Duration::ZERO);
        assert_eq!(p2.state, PollState::Ok);
    }

    #[test]
    fn poll_wakes_when_an_event_arrives() {
        let log = Arc::new(EventLog::new());
        let (epoch, next) = log.subscribe(filt());
        let l2 = log.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            l2.push(EventKind::Changed, false, r"C:\x.txt".into(), String::new());
        });
        let start = Instant::now();
        let p = log.poll(epoch, next, 10, Duration::from_secs(5));
        assert_eq!(p.events.len(), 1);
        assert!(start.elapsed() < Duration::from_secs(2));
        t.join().unwrap();
    }

    #[test]
    fn unsubscribed_log_reports_it() {
        let log = EventLog::new();
        let p = log.poll(log.epoch, 1, 1, Duration::ZERO);
        assert_eq!(p.state, PollState::NotSubscribed);
    }
}
