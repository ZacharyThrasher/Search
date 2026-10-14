//! Background thread that talks to the helper, so the window never waits on
//! the pipe. Only the newest query matters: older ones still queued are dropped.

use std::os::windows::process::CommandExt;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use search::ipc::{HitMsg, KindFilter, Reply, Request};
use search::pipe::Client;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::WM_APP_RESULT;

pub enum Job {
    /// An empty `text` just forgets the previous query.
    Query { seq: u64, text: String, limit: u16 },
    /// The popup was opened: connect (and start the helper) ahead of the first key.
    Warm,
}

pub enum Outcome {
    Hits { total: u64, hits: Vec<HitMsg> },
    /// The helper is up but still loading its index.
    Loading(String),
    /// The helper is not reachable yet; the worker keeps trying.
    Offline(String),
    /// Something that retrying won't fix.
    Error(String),
}

pub struct Answer {
    pub seq: u64,
    pub outcome: Outcome,
}

pub type Slot = Arc<Mutex<Option<Answer>>>;

pub struct Worker {
    tx: Sender<Job>,
}

impl Worker {
    pub fn start(hwnd: HWND, slot: Slot, pipe_name: String) -> Worker {
        let (tx, rx) = channel();
        let hwnd = hwnd as usize;
        std::thread::Builder::new()
            .name("helper-client".into())
            .spawn(move || run(rx, hwnd, slot, pipe_name))
            .expect("spawn worker");
        Worker { tx }
    }

    pub fn send(&self, job: Job) {
        let _ = self.tx.send(job);
    }
}

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Ask Task Scheduler to start the helper's task. `Err` carries a message for
/// cases where retrying won't help.
fn start_helper() -> Result<(), String> {
    let out = std::process::Command::new("schtasks.exe")
        .args(["/Run", "/TN", "SearchHelper"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(_) => Err("The search helper isn't installed. Run scripts\\install.ps1 once.".into()),
        Err(e) => Err(format!("Couldn't start the search helper: {e}")),
    }
}

struct Link {
    name: String,
    client: Option<Client>,
    last_start: Instant,
}

impl Link {
    /// Connect if needed. `Err` is what to show while we wait or give up.
    fn ensure(&mut self, wait: Duration) -> Result<&mut Client, Outcome> {
        if self.client.is_none() {
            match Client::connect(&self.name, wait) {
                Ok(c) => self.client = Some(c),
                Err(_) if self.last_start.elapsed() > Duration::from_secs(15) => {
                    self.last_start = Instant::now();
                    return Err(match start_helper() {
                        Ok(()) => Outcome::Offline("Starting the search helper…".into()),
                        Err(msg) => Outcome::Error(msg),
                    });
                }
                Err(_) => return Err(Outcome::Offline("Waiting for the search helper…".into())),
            }
        }
        Ok(self.client.as_mut().unwrap())
    }

    fn search(&mut self, text: &str, limit: u16) -> Outcome {
        for attempt in 0..2 {
            let client = match self.ensure(Duration::from_millis(if attempt == 0 { 150 } else { 50 })) {
                Ok(c) => c,
                Err(o) => return o,
            };
            let req = Request::Search {
                query: text.to_string(),
                limit,
                kind: KindFilter::Any,
            };
            match client.request(&req) {
                Ok(Reply::Hits { total, hits }) => return Outcome::Hits { total, hits },
                Ok(Reply::Loading(why)) => return Outcome::Loading(why),
                Ok(Reply::Error(e)) => return Outcome::Error(e),
                Ok(_) => return Outcome::Error("unexpected reply from the helper".into()),
                // The helper restarted or went away: reconnect once.
                Err(_) => self.client = None,
            }
        }
        Outcome::Offline("Lost the search helper; reconnecting…".into())
    }

    /// Check the protocol version once connected.
    fn warm(&mut self) -> Option<Outcome> {
        let client = match self.ensure(Duration::from_millis(150)) {
            Ok(c) => c,
            Err(o) => return Some(o),
        };
        match client.request(&Request::Status) {
            Ok(Reply::Status(s)) if s.version != search::ipc::PROTOCOL_VERSION => Some(Outcome::Error(
                "The search helper is an older version. Re-run scripts\\install.ps1.".into(),
            )),
            Ok(_) => None,
            Err(_) => {
                self.client = None;
                None
            }
        }
    }
}

fn run(rx: Receiver<Job>, hwnd: usize, slot: Slot, name: String) {
    let publish = |seq: u64, outcome: Outcome| {
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(Answer { seq, outcome });
        // SAFETY: posting to a window handle; if the window is gone this just fails.
        unsafe { PostMessageW(hwnd as HWND, WM_APP_RESULT, 0, 0) };
    };
    let mut link = Link {
        name,
        client: None,
        last_start: Instant::now() - Duration::from_secs(60),
    };
    // The query to repeat while the helper is warming up.
    let mut last: Option<(u64, String, u16)> = None;
    let mut retrying = false;

    loop {
        let first = if retrying {
            match rx.recv_timeout(Duration::from_millis(700)) {
                Ok(j) => Some(j),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(j) => Some(j),
                Err(_) => return,
            }
        };
        let mut warm = false;
        let mut next = first;
        while let Some(job) = next.take() {
            match job {
                Job::Query { seq, text, limit } => {
                    last = (!text.trim().is_empty()).then_some((seq, text, limit));
                    retrying = false;
                }
                Job::Warm => warm = true,
            }
            next = rx.try_recv().ok();
        }

        match &last {
            Some((seq, text, limit)) => {
                let outcome = link.search(text, *limit);
                retrying = matches!(outcome, Outcome::Offline(_) | Outcome::Loading(_));
                publish(*seq, outcome);
            }
            // Nothing is on screen to attach a problem to; it surfaces on the first key.
            None if warm => {
                let _ = link.warm();
            }
            None => {}
        }
    }
}
