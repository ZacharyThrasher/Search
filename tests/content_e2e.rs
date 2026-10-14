//! The whole content pipeline against a fake helper: initial crawl through
//! `ListFiles`, live updates through `Events`, real extractor child processes.

#![cfg(windows)]

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use search::content::config::ContentConfig;
use search::content::indexer::{self, Options, Running};
use search::content::search::ContentReader;
use search::events::{Event, EventKind, Filter, PollState};
use search::ipc::{ListCursor, Reply, Request};
use search::pipe;

/// Stands in for searchd: lists files under a folder and replays queued events.
struct FakeHelper {
    root: PathBuf,
    epoch: AtomicU64,
    events: Mutex<VecDeque<Event>>,
    cv: Condvar,
    next_seq: AtomicU64,
    subscriptions: AtomicU64,
    /// Answer the next `Events` poll with "the helper restarted".
    force_new_epoch: Mutex<bool>,
}

impl FakeHelper {
    fn start(root: &Path, tag: &str) -> (Arc<FakeHelper>, String) {
        let h = Arc::new(FakeHelper {
            root: root.to_path_buf(),
            epoch: AtomicU64::new(7),
            events: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            next_seq: AtomicU64::new(1),
            subscriptions: AtomicU64::new(0),
            force_new_epoch: Mutex::new(false),
        });
        let name = pipe::pipe_name(Some(tag)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (h2, n2) = (h.clone(), name.clone());
        std::thread::spawn(move || {
            let handler: Arc<dyn Fn(Request) -> Reply + Send + Sync> = Arc::new(move |r| h2.handle(r));
            let _ = pipe::serve(&n2, handler, move || tx.send(()).unwrap());
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        (h, name)
    }

    fn push(&self, kind: EventKind, is_dir: bool, path: &Path, old: Option<&Path>) {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push_back(Event {
            seq,
            kind,
            is_dir,
            path: path.to_string_lossy().into_owned(),
            old_path: old.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        });
        self.cv.notify_all();
    }

    fn list(&self, f: &Filter) -> Vec<String> {
        fn walk(dir: &Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else {
                    out.push(p.to_string_lossy().into_owned());
                }
            }
        }
        let mut all = Vec::new();
        walk(&self.root, &mut all);
        all.into_iter().filter(|p| f.allows_file(p)).collect()
    }

    fn handle(&self, r: Request) -> Reply {
        match r {
            Request::Subscribe(_) => {
                self.subscriptions.fetch_add(1, Ordering::SeqCst);
                Reply::Subscribed { epoch: self.epoch.load(Ordering::SeqCst), next: self.next_seq.load(Ordering::SeqCst) }
            }
            Request::Events { epoch, next, wait_ms, .. } => {
                let end = Instant::now() + Duration::from_millis(wait_ms.min(500) as u64);
                loop {
                    if std::mem::take(&mut *self.force_new_epoch.lock().unwrap()) {
                        self.epoch.fetch_add(1, Ordering::SeqCst);
                    }
                    let cur = self.epoch.load(Ordering::SeqCst);
                    if epoch != cur {
                        return Reply::Events { state: PollState::NewEpoch, epoch: cur, next: 1, events: vec![] };
                    }
                    let g = self.events.lock().unwrap();
                    let ev: Vec<Event> = g.iter().filter(|e| e.seq >= next).cloned().collect();
                    if !ev.is_empty() {
                        let after = ev.last().unwrap().seq + 1;
                        return Reply::Events { state: PollState::Ok, epoch, next: after, events: ev };
                    }
                    if Instant::now() >= end {
                        return Reply::Events { state: PollState::Ok, epoch, next, events: vec![] };
                    }
                    let _ = self.cv.wait_timeout(g, Duration::from_millis(50)).unwrap();
                }
            }
            Request::ListFiles { filter, cursor } => {
                assert_eq!(cursor, ListCursor::default());
                Reply::Files { restart: false, next: None, paths: self.list(&filter) }
            }
            _ => Reply::Done,
        }
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn docx(path: &Path, text: &str) {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut w = zip::ZipWriter::new(&mut buf);
        w.start_file("word/document.xml", zip::write::SimpleFileOptions::default()).unwrap();
        write!(w, "<w:document xmlns:w=\"x\"><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>").unwrap();
        w.finish().unwrap();
    }
    std::fs::write(path, buf.into_inner()).unwrap();
}

/// A one-page PDF whose text is `text` (plain ASCII, no escapes needed).
fn pdf(path: &Path, text: &str) {
    let content = format!("BT /F1 18 Tf 72 720 Td ({text}) Tj ET");
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_string(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_string(),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
    std::fs::write(path, out).unwrap();
}

fn options(dir: &Path, pipe: Option<String>) -> Options {
    // The test folders live under AppData\Local\Temp, which the defaults skip.
    let cfg = ContentConfig {
        workers: 2,
        file_timeout_secs: 2,
        default_excludes: false,
        skip_dot_folders: false,
        exclude_folders: vec!["node_modules".into()],
        ..Default::default()
    };
    let mut o = Options::new(dir.to_path_buf(), cfg, PathBuf::from(env!("CARGO_BIN_EXE_searchi")));
    o.pipe_name = pipe;
    o.gate = false;
    if std::env::var_os("E2E_LOG").is_some() {
        o.log = Arc::new(|s: &str| eprintln!("[indexer] {s}"));
    }
    o.commit_every = Duration::from_millis(200);
    o
}

fn hits(dir: &Path, q: &str) -> Vec<String> {
    match ContentReader::open(dir) {
        Ok(r) => r.search(q, 20).into_iter().map(|h| h.path).collect(),
        Err(_) => Vec::new(),
    }
}

fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for: {what}");
}

fn idle(r: &Running) {
    assert!(r.wait_idle(Duration::from_secs(30)), "indexer did not settle: {:?}", r.progress());
}

#[test]
fn crawl_then_live_updates_through_a_fake_helper() {
    let work = tempfile::tempdir().unwrap();
    let root = work.path().join("files");
    let index = work.path().join("content");
    write(&root.join("a.txt"), "the quarterly zebra report");
    write(&root.join("sub").join("b.md"), "# Notes\nthe llama farm budget");
    write(&root.join("sub").join("d.txt"), "a lonely walrus on the ice");
    write(&root.join("skip.exe"), "zebra but not an indexed type");
    write(&root.join("node_modules").join("x.txt"), "zebra in an excluded folder");
    write(&root.join("blob.txt"), "\u{0}\u{1}\u{2}\u{0}\u{0}\u{3}binary\u{0}\u{0}");
    docx(&root.join("c.docx"), "contract about the okapi merger");
    pdf(&root.join("p.pdf"), "Hello giraffe from a PDF page");

    let (helper, name) = FakeHelper::start(&root, &format!("e2e-{}", std::process::id()));
    let r = indexer::start(options(&index, Some(name))).unwrap();
    idle(&r);

    // The first crawl found every kind of document and left the rest out.
    assert_eq!(hits(&index, "zebra"), [root.join("a.txt").to_string_lossy()]);
    assert_eq!(hits(&index, "llama").len(), 1);
    assert_eq!(hits(&index, "okapi")[0], root.join("c.docx").to_string_lossy());
    assert_eq!(hits(&index, "giraffe")[0], root.join("p.pdf").to_string_lossy());
    assert!(hits(&index, "binary").is_empty());
    let p = r.progress();
    assert_eq!(p.indexed, 5, "{p:?}"); // a, b, d, docx, pdf
    assert_eq!(p.skipped, 1, "{p:?}"); // blob.txt
    assert!(helper.subscriptions.load(Ordering::SeqCst) >= 1);

    // A file is rewritten: the new words appear, the old ones go.
    write(&root.join("a.txt"), "now about a hippopotamus instead");
    helper.push(EventKind::Changed, false, &root.join("a.txt"), None);
    eventually("rewritten file indexed", || !hits(&index, "hippopotamus").is_empty());
    eventually("old text gone", || hits(&index, "zebra").is_empty());

    // Deleted file.
    std::fs::remove_file(root.join("sub").join("b.md")).unwrap();
    helper.push(EventKind::Deleted, false, &root.join("sub").join("b.md"), None);
    eventually("deleted file removed", || hits(&index, "llama").is_empty());

    // A new file, announced by an event.
    write(&root.join("new.txt"), "fresh text about a pangolin");
    helper.push(EventKind::Changed, false, &root.join("new.txt"), None);
    eventually("new file indexed", || !hits(&index, "pangolin").is_empty());

    // A renamed file keeps its text under the new name.
    std::fs::rename(root.join("new.txt"), root.join("renamed.txt")).unwrap();
    helper.push(EventKind::Renamed, false, &root.join("renamed.txt"), Some(&root.join("new.txt")));
    eventually("renamed file", || hits(&index, "pangolin") == [root.join("renamed.txt").to_string_lossy()]);

    // A renamed folder: everything under the old name goes, the new one is crawled.
    std::fs::rename(root.join("sub"), root.join("sub2")).unwrap();
    helper.push(EventKind::Renamed, true, &root.join("sub2"), Some(&root.join("sub")));
    eventually("folder rename", || hits(&index, "walrus") == [root.join("sub2").join("d.txt").to_string_lossy()]);

    // The helper restarts (new epoch): the indexer subscribes again and
    // catches up on what it missed by comparing disk and index.
    write(&root.join("missed.txt"), "written while nobody was listening: capybara");
    std::fs::remove_file(root.join("c.docx")).unwrap();
    *helper.force_new_epoch.lock().unwrap() = true;
    eventually("catch-up after helper restart", || {
        !hits(&index, "capybara").is_empty() && hits(&index, "okapi").is_empty()
    });
    assert!(helper.subscriptions.load(Ordering::SeqCst) >= 2);

    r.stop();
    // Everything survives a restart without re-reading unchanged files.
    let r2 = indexer::start(options(&index, None)).unwrap();
    assert_eq!(r2.progress().indexed, 5, "{:?}", r2.progress()); // a, d, p, renamed, missed
    r2.stop();
}

#[test]
fn stuck_and_crashing_files_cost_only_themselves() {
    let work = tempfile::tempdir().unwrap();
    let root = work.path().join("files");
    let index = work.path().join("content");
    write(&root.join("hang.txt"), "this parser will never come back");
    write(&root.join("crash.txt"), "this parser will fall over");
    for i in 0..6 {
        write(&root.join(format!("ok{i}.txt")), &format!("ordinary file number {i} mentions the aardvark"));
    }
    // SAFETY: set before any thread of this test starts a child; no other test reads these names.
    unsafe {
        std::env::set_var("SEARCHI_TEST_HANG", "hang.txt");
        std::env::set_var("SEARCHI_TEST_CRASH", "crash.txt");
    }
    let mut o = options(&index, None);
    o.walk_roots = vec![root.clone()];
    o.cfg.workers = 1; // one extractor, so it has to be replaced after each failure
    let r = indexer::start(o).unwrap();
    idle(&r);
    let p = r.progress();
    assert_eq!(p.indexed, 6, "{p:?}");
    assert_eq!(p.failed, 2, "{p:?}"); // timeout + crash
    assert_eq!(hits(&index, "aardvark").len(), 6);
    r.stop();

    // They are not retried while unchanged (a restart would otherwise hang again for 2 s each).
    let t = Instant::now();
    let mut o = options(&index, None);
    o.walk_roots = vec![root.clone()];
    o.cfg.workers = 1;
    let r = indexer::start(o).unwrap();
    idle(&r);
    assert!(t.elapsed() < Duration::from_secs(4), "unchanged failures were retried");
    assert_eq!(r.progress().failed, 2);
    r.stop();
}

#[test]
fn walking_mode_removes_files_deleted_while_stopped() {
    let work = tempfile::tempdir().unwrap();
    let root = work.path().join("files");
    let index = work.path().join("content");
    write(&root.join("keep.txt"), "kept words about marmots");
    write(&root.join("drop.txt"), "dropped words about marmots too");
    let mut o = options(&index, None);
    o.walk_roots = vec![root.clone()];
    let r = indexer::start(o).unwrap();
    idle(&r);
    assert_eq!(hits(&index, "marmots").len(), 2);
    r.stop();

    std::fs::remove_file(root.join("drop.txt")).unwrap();
    write(&root.join("keep.txt"), "kept words about otters now");
    let mut o = options(&index, None);
    o.walk_roots = vec![root.clone()];
    let r = indexer::start(o).unwrap();
    idle(&r);
    r.flush();
    assert!(hits(&index, "marmots").is_empty());
    assert_eq!(hits(&index, "otters").len(), 1);
    r.stop();
}

/// The extractor children really do run at the lowest CPU, I/O and memory priority.
#[test]
fn extractor_children_run_at_background_priority() {
    use std::process::{Command, Stdio};
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetPriorityClass, GetProcessInformation, IDLE_PRIORITY_CLASS, OpenProcess};

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQueryInformationProcess(h: *mut core::ffi::c_void, class: u32, info: *mut core::ffi::c_void, len: u32, ret: *mut u32) -> i32;
    }

    let mut child = Command::new(env!("CARGO_BIN_EXE_searchi"))
        .arg("--worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(700)); // let it start up and lower itself
    // SAFETY: query-only calls on a child we own; the handle is closed.
    unsafe {
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, child.id());
        assert!(!h.is_null());
        assert_eq!(GetPriorityClass(h), IDLE_PRIORITY_CLASS, "CPU class");
        let mut io = 99u32;
        let mut ret = 0u32;
        assert_eq!(NtQueryInformationProcess(h, 33, &mut io as *mut u32 as *mut _, 4, &mut ret), 0);
        assert_eq!(io, 0, "I/O priority (0 = very low)");
        let mut mem = [99u32; 1];
        assert_ne!(GetProcessInformation(h, 0, mem.as_mut_ptr().cast(), 4), 0);
        assert_eq!(mem[0], 1, "memory priority (1 = very low)");
        CloseHandle(h);
    }
    drop(child.stdin.take()); // the child exits when its input closes
    let _ = child.wait();
}
