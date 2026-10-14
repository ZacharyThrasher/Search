//! Extractor child processes.
//!
//! File parsers read untrusted data, so the indexer never runs them itself.
//! It starts `searchi --worker` children (low priority, inside a job object
//! with a memory limit, killed with the indexer) and talks to each over its
//! stdin/stdout: one request per file, one reply back. A child that runs past
//! the time limit, crashes or runs out of memory costs only that file: it is
//! killed, replaced, and the file recorded as failed.

use std::io::{self, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

use super::chunk::{Chunk, chunk_segments};
use super::extract::{self, Skip, Status};
use super::sys::Job;

/// Largest frame either side accepts (a file's text is capped at a few MB).
const MAX_FRAME: usize = 256 << 20;

/// What a child sends back for one file.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Text { chunks: Vec<Chunk>, truncated: bool },
    Skipped(Skip),
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

struct Cur<'a>(&'a [u8]);

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn str(&mut self) -> io::Result<String> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| io::ErrorKind::InvalidData.into())
    }
}

pub fn encode_request(path: &str, max_bytes: u64, chunk_bytes: u32) -> Vec<u8> {
    let mut out = vec![1u8];
    put_u64(&mut out, max_bytes);
    put_u32(&mut out, chunk_bytes);
    put_str(&mut out, path);
    out
}

pub fn decode_request(buf: &[u8]) -> io::Result<(String, u64, u32)> {
    let mut c = Cur(buf);
    if c.u8()? != 1 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let max = c.u64()?;
    let chunk = c.u32()?;
    Ok((c.str()?, max, chunk))
}

pub fn encode_reply(r: &Reply) -> Vec<u8> {
    let mut out = Vec::new();
    match r {
        Reply::Text { chunks, truncated } => {
            out.push(0);
            out.push(*truncated as u8);
            put_u32(&mut out, chunks.len() as u32);
            for c in chunks {
                put_u64(&mut out, c.start);
                put_u64(&mut out, c.end);
                put_u32(&mut out, c.page);
                put_str(&mut out, &c.text);
            }
        }
        Reply::Skipped(s) => {
            out.push(1);
            let (kind, msg) = match s {
                Skip::Binary => (0, ""),
                Skip::Empty => (1, ""),
                Skip::TooBig => (2, ""),
                Skip::Unsupported => (3, ""),
                Skip::Encrypted => (4, ""),
                Skip::Failed(m) => (5, m.as_str()),
                Skip::Unreadable(m) => (6, m.as_str()),
            };
            out.push(kind);
            put_str(&mut out, msg);
        }
    }
    out
}

pub fn decode_reply(buf: &[u8]) -> io::Result<Reply> {
    let mut c = Cur(buf);
    match c.u8()? {
        0 => {
            let truncated = c.u8()? != 0;
            let n = c.u32()? as usize;
            let mut chunks = Vec::with_capacity(n.min(65_536));
            for _ in 0..n {
                chunks.push(Chunk { start: c.u64()?, end: c.u64()?, page: c.u32()?, text: c.str()? });
            }
            Ok(Reply::Text { chunks, truncated })
        }
        1 => {
            let kind = c.u8()?;
            let msg = c.str()?;
            Ok(Reply::Skipped(match kind {
                0 => Skip::Binary,
                1 => Skip::Empty,
                2 => Skip::TooBig,
                3 => Skip::Unsupported,
                4 => Skip::Encrypted,
                5 => Skip::Failed(msg),
                6 => Skip::Unreadable(msg),
                _ => return Err(io::ErrorKind::InvalidData.into()),
            }))
        }
        _ => Err(io::ErrorKind::InvalidData.into()),
    }
}

fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// Extract and chunk one file, as the child does.
pub fn extract_and_chunk(path: &str, max_bytes: u64, chunk_bytes: usize) -> Reply {
    // Test hooks (only active when the test sets the variable): a file that
    // hangs the parser and one that kills the process.
    if let Ok(name) = std::env::var("SEARCHI_TEST_HANG")
        && path.ends_with(&name)
    {
        std::thread::sleep(Duration::from_secs(3600));
    }
    if let Ok(name) = std::env::var("SEARCHI_TEST_CRASH")
        && path.ends_with(&name)
    {
        std::process::abort();
    }
    let r = std::panic::catch_unwind(|| extract::extract(Path::new(path), max_bytes));
    match r {
        Ok(Ok(segments)) => {
            let (chunks, truncated) = chunk_segments(&segments, chunk_bytes);
            if chunks.is_empty() { Reply::Skipped(Skip::Empty) } else { Reply::Text { chunks, truncated } }
        }
        Ok(Err(skip)) => Reply::Skipped(skip),
        Err(_) => Reply::Skipped(Skip::Failed("extractor panicked".into())),
    }
}

/// The child's main loop: serve requests from stdin until it closes.
pub fn run_child() -> io::Result<()> {
    super::sys::enter_background_mode();
    let stdin = io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut out = stdout.lock();
    // Parsers can recurse deeply on hostile input; give them a big stack.
    while let Some(frame) = read_frame(&mut input)? {
        let (path, max, chunk) = decode_request(&frame)?;
        let reply = std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(move || extract_and_chunk(&path, max, chunk as usize))
            .map_err(io::Error::other)?
            .join()
            .unwrap_or_else(|_| Reply::Skipped(Skip::Failed("extractor crashed".into())));
        write_frame(&mut out, &encode_reply(&reply))?;
    }
    Ok(())
}

/// What the parent concludes about a file after asking a child.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Text { chunks: Vec<Chunk>, truncated: bool },
    Skipped(Skip),
    /// The child ran past the time limit and was killed.
    TimedOut,
    /// The child died (crash, out of memory, killed) while working on the file.
    Crashed,
}

impl Outcome {
    pub fn status(&self) -> Status {
        match self {
            Outcome::Text { truncated: true, .. } => Status::Partial,
            Outcome::Text { .. } => Status::Indexed,
            Outcome::Skipped(s) => s.status(),
            Outcome::TimedOut => Status::Timeout,
            Outcome::Crashed => Status::Failed,
        }
    }
}

/// One extractor child, started on first use and replaced when it dies.
pub struct Extractor {
    exe: std::path::PathBuf,
    job: Option<Arc<Job>>,
    live: Option<Live>,
}

struct Live {
    child: Child,
    stdin: ChildStdin,
    replies: Receiver<io::Result<Option<Vec<u8>>>>,
}

impl Extractor {
    pub fn new(exe: std::path::PathBuf, job: Option<Arc<Job>>) -> Extractor {
        Extractor { exe, job, live: None }
    }

    fn start(&mut self) -> io::Result<()> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const IDLE_PRIORITY_CLASS: u32 = 0x0000_0040;
        let mut child = Command::new(&self.exe)
            .arg("--worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW | IDLE_PRIORITY_CLASS)
            .spawn()?;
        if let Some(job) = &self.job {
            job.assign(&child);
        }
        let stdin = child.stdin.take().ok_or_else(|| io::Error::other("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| io::Error::other("no stdout"))?;
        let (tx, rx) = channel();
        std::thread::Builder::new().name("extractor-reader".into()).spawn(move || {
            let mut r = BufReader::new(stdout);
            loop {
                let frame = read_frame(&mut r);
                let done = !matches!(frame, Ok(Some(_)));
                if tx.send(frame).is_err() || done {
                    break;
                }
            }
        })?;
        self.live = Some(Live { child, stdin, replies: rx });
        Ok(())
    }

    /// Kill the child (if any); the next call starts a fresh one.
    pub fn kill(&mut self) {
        if let Some(mut l) = self.live.take() {
            let _ = l.child.kill();
            let _ = l.child.wait();
        }
    }

    pub fn running(&self) -> bool {
        self.live.is_some()
    }

    pub fn extract(&mut self, path: &str, max_bytes: u64, chunk_bytes: u32, timeout: Duration) -> Outcome {
        for attempt in 0..2 {
            if self.live.is_none()
                && let Err(e) = self.start()
            {
                return Outcome::Skipped(Skip::Unreadable(format!("cannot start extractor: {e}")));
            }
            let live = self.live.as_mut().unwrap();
            let sent = write_frame(&mut live.stdin, &encode_request(path, max_bytes, chunk_bytes));
            if sent.is_err() {
                // The child was already dead (it quit between files): start another once.
                self.kill();
                if attempt == 0 {
                    continue;
                }
                return Outcome::Crashed;
            }
            return match live.replies.recv_timeout(timeout) {
                Ok(Ok(Some(buf))) => match decode_reply(&buf) {
                    Ok(Reply::Text { chunks, truncated }) => Outcome::Text { chunks, truncated },
                    Ok(Reply::Skipped(s)) => Outcome::Skipped(s),
                    Err(_) => {
                        self.kill();
                        Outcome::Crashed
                    }
                },
                Ok(Ok(None)) | Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => {
                    self.kill();
                    Outcome::Crashed
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.kill();
                    Outcome::TimedOut
                }
            };
        }
        Outcome::Crashed
    }
}

impl Drop for Extractor {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let req = encode_request(r"C:\docs\Résumé.pdf", 20 << 20, 1200);
        assert_eq!(decode_request(&req).unwrap(), (r"C:\docs\Résumé.pdf".to_string(), 20 << 20, 1200));
        for r in [
            Reply::Text {
                chunks: vec![Chunk { start: 0, end: 10, page: 1, text: "hello wörld".into() }],
                truncated: true,
            },
            Reply::Skipped(Skip::Binary),
            Reply::Skipped(Skip::Failed("boom".into())),
            Reply::Skipped(Skip::Unreadable("locked".into())),
            Reply::Skipped(Skip::Encrypted),
        ] {
            assert_eq!(decode_reply(&encode_reply(&r)).unwrap(), r);
        }
    }

    #[test]
    fn damaged_messages_are_errors_not_panics() {
        let r = encode_reply(&Reply::Text { chunks: vec![Chunk { start: 1, end: 2, page: 0, text: "abc".into() }], truncated: false });
        for cut in 0..r.len() {
            let _ = decode_reply(&r[..cut]);
        }
        assert!(decode_reply(&[9]).is_err());
        assert!(decode_reply(&[1, 77, 0, 0, 0, 0]).is_err());
        assert!(decode_request(&[2]).is_err());
        let mut huge = vec![0u8, 0];
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_reply(&huge).is_err());
    }

    #[test]
    fn extraction_in_process_matches_what_the_child_sends() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "one two three").unwrap();
        match extract_and_chunk(p.to_str().unwrap(), 1 << 20, 1200) {
            Reply::Text { chunks, truncated } => {
                assert!(!truncated);
                assert_eq!(chunks.len(), 1);
                assert_eq!(chunks[0].text, "one two three");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            extract_and_chunk(dir.path().join("none.txt").to_str().unwrap(), 100, 1200),
            Reply::Skipped(Skip::Unreadable(_))
        ));
    }
}
