//! Wire format between the popup and the elevated helper.
//!
//! Every message is a frame: a little-endian `u32` length, then that many
//! payload bytes. A request starts with an op byte; a reply starts with a
//! status byte. Nothing here touches the OS, so it is tested directly.
//!
//! The helper never executes anything on a client's behalf: it answers
//! queries and a few housekeeping ops, and the unelevated popup does the
//! launching itself.

use std::io::{self, Read, Write};

/// Bumped when the format changes; the popup refuses a helper that disagrees.
pub const PROTOCOL_VERSION: u16 = 1;

/// Largest frame either side will accept (a full page of long paths is ~30 KB).
pub const MAX_FRAME: usize = 1 << 20;
pub const MAX_QUERY_BYTES: usize = 1024;
pub const MAX_LIMIT: usize = 500;

const OP_STATUS: u8 = 1;
const OP_SEARCH: u8 = 2;
const OP_SAVE: u8 = 3;
const OP_SHUTDOWN: u8 = 4;

pub const STATUS_OK: u8 = 0;
/// The index is still loading; the body is a progress line.
pub const STATUS_LOADING: u8 = 1;
/// The request was refused; the body is a message.
pub const STATUS_ERROR: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindFilter {
    Any,
    Files,
    Dirs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Status,
    Search {
        query: String,
        limit: u16,
        kind: KindFilter,
    },
    /// Write snapshots now (even if little changed).
    Save,
    /// Save and exit.
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitMsg {
    pub path: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusMsg {
    pub version: u16,
    pub entries: u64,
    pub volumes: u16,
    pub elevated: bool,
    /// Any volume is served from an old snapshot with no live updates.
    pub stale: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Status(StatusMsg),
    Hits { total: u64, hits: Vec<HitMsg> },
    Done,
    /// Index still loading (`detail` says how far).
    Loading(String),
    Error(String),
}

#[derive(Debug)]
pub struct BadMessage(pub &'static str);

impl std::fmt::Display for BadMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bad message: {}", self.0)
    }
}

impl std::error::Error for BadMessage {}

fn bad<T>(why: &'static str) -> Result<T, BadMessage> {
    Err(BadMessage(why))
}

struct Cur<'a>(&'a [u8]);

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], BadMessage> {
        if self.0.len() < n {
            return bad("truncated");
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, BadMessage> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, BadMessage> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, BadMessage> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, BadMessage> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn str(&mut self, max: usize) -> Result<String, BadMessage> {
        let n = self.u32()? as usize;
        if n > max {
            return bad("string too long");
        }
        String::from_utf8(self.take(n)?.to_vec()).or_else(|_| bad("not utf-8"))
    }
    fn done(&self) -> Result<(), BadMessage> {
        if self.0.is_empty() { Ok(()) } else { bad("trailing bytes") }
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Request::Status => out.push(OP_STATUS),
            Request::Search { query, limit, kind } => {
                out.push(OP_SEARCH);
                out.extend_from_slice(&limit.to_le_bytes());
                out.push(match kind {
                    KindFilter::Any => 0,
                    KindFilter::Files => 1,
                    KindFilter::Dirs => 2,
                });
                put_str(&mut out, query);
            }
            Request::Save => out.push(OP_SAVE),
            Request::Shutdown => out.push(OP_SHUTDOWN),
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Request, BadMessage> {
        let mut c = Cur(buf);
        let r = match c.u8()? {
            OP_STATUS => Request::Status,
            OP_SEARCH => {
                let limit = c.u16()?;
                let kind = match c.u8()? {
                    0 => KindFilter::Any,
                    1 => KindFilter::Files,
                    2 => KindFilter::Dirs,
                    _ => return bad("unknown kind"),
                };
                let query = c.str(MAX_QUERY_BYTES)?;
                Request::Search { query, limit, kind }
            }
            OP_SAVE => Request::Save,
            OP_SHUTDOWN => Request::Shutdown,
            _ => return bad("unknown op"),
        };
        c.done()?;
        Ok(r)
    }
}

impl Reply {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Reply::Status(s) => {
                out.push(STATUS_OK);
                out.push(1); // body kind: status
                out.extend_from_slice(&s.version.to_le_bytes());
                out.extend_from_slice(&s.entries.to_le_bytes());
                out.extend_from_slice(&s.volumes.to_le_bytes());
                out.push(s.elevated as u8 | (s.stale as u8) << 1);
                put_str(&mut out, &s.detail);
            }
            Reply::Hits { total, hits } => {
                out.push(STATUS_OK);
                out.push(2); // body kind: hits
                out.extend_from_slice(&total.to_le_bytes());
                out.extend_from_slice(&(hits.len() as u32).to_le_bytes());
                for h in hits {
                    out.push(h.is_dir as u8);
                    put_str(&mut out, &h.path);
                }
            }
            Reply::Done => {
                out.push(STATUS_OK);
                out.push(3);
            }
            Reply::Loading(s) => {
                out.push(STATUS_LOADING);
                put_str(&mut out, s);
            }
            Reply::Error(s) => {
                out.push(STATUS_ERROR);
                put_str(&mut out, s);
            }
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Reply, BadMessage> {
        let mut c = Cur(buf);
        let r = match c.u8()? {
            STATUS_OK => match c.u8()? {
                1 => {
                    let version = c.u16()?;
                    let entries = c.u64()?;
                    let volumes = c.u16()?;
                    let flags = c.u8()?;
                    let detail = c.str(MAX_FRAME)?;
                    Reply::Status(StatusMsg {
                        version,
                        entries,
                        volumes,
                        elevated: flags & 1 != 0,
                        stale: flags & 2 != 0,
                        detail,
                    })
                }
                2 => {
                    let total = c.u64()?;
                    let n = c.u32()? as usize;
                    if n > MAX_LIMIT {
                        return bad("too many hits");
                    }
                    let mut hits = Vec::with_capacity(n);
                    for _ in 0..n {
                        let is_dir = c.u8()? != 0;
                        let path = c.str(MAX_FRAME)?;
                        hits.push(HitMsg { path, is_dir });
                    }
                    Reply::Hits { total, hits }
                }
                3 => Reply::Done,
                _ => return bad("unknown reply body"),
            },
            STATUS_LOADING => Reply::Loading(c.str(MAX_FRAME)?),
            STATUS_ERROR => Reply::Error(c.str(MAX_FRAME)?),
            _ => return bad("unknown status"),
        };
        c.done()?;
        Ok(r)
    }
}

/// Write one frame.
pub fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
    }
    // One write, so a pipe sees the length and body together.
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf)?;
    w.flush()
}

/// Read one frame. `Ok(None)` is a clean end of stream between frames.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        for r in [
            Request::Status,
            Request::Save,
            Request::Shutdown,
            Request::Search {
                query: "résumé final".into(),
                limit: 50,
                kind: KindFilter::Dirs,
            },
            Request::Search {
                query: String::new(),
                limit: 0,
                kind: KindFilter::Any,
            },
        ] {
            assert_eq!(Request::decode(&r.encode()).unwrap(), r);
        }
    }

    #[test]
    fn replies_round_trip() {
        for r in [
            Reply::Done,
            Reply::Loading("C: scanning".into()),
            Reply::Error("nope".into()),
            Reply::Status(StatusMsg {
                version: PROTOCOL_VERSION,
                entries: 15_700_000,
                volumes: 2,
                elevated: true,
                stale: false,
                detail: "C: 12 changes applied".into(),
            }),
            Reply::Hits {
                total: 123_456,
                hits: vec![
                    HitMsg { path: "C:\\Windows\\notepad.exe".into(), is_dir: false },
                    HitMsg { path: "C:\\Users\\Zach\\Documents".into(), is_dir: true },
                ],
            },
        ] {
            assert_eq!(Reply::decode(&r.encode()).unwrap(), r);
        }
    }

    #[test]
    fn hostile_input_is_refused_not_trusted() {
        // Truncations of every valid message never panic and never decode.
        let msgs = [
            Request::Search { query: "abc".into(), limit: 5, kind: KindFilter::Any }.encode(),
            Reply::Hits { total: 1, hits: vec![HitMsg { path: "C:\\a".into(), is_dir: false }] }.encode(),
        ];
        for m in &msgs {
            for cut in 0..m.len() {
                let _ = Request::decode(&m[..cut]);
                let _ = Reply::decode(&m[..cut]);
            }
        }
        assert!(Request::decode(&[]).is_err());
        assert!(Request::decode(&[99]).is_err());
        assert!(Request::decode(&[OP_STATUS, 0]).is_err()); // trailing byte
        // A string length that claims more than the frame (or more than allowed).
        let mut huge = vec![OP_SEARCH, 5, 0, 0];
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(Request::decode(&huge).is_err());
        let mut long = vec![OP_SEARCH, 5, 0, 0];
        long.extend_from_slice(&((MAX_QUERY_BYTES + 1) as u32).to_le_bytes());
        long.extend(std::iter::repeat_n(b'a', MAX_QUERY_BYTES + 1));
        assert!(Request::decode(&long).is_err());
        // Bad utf-8 and an absurd hit count.
        let mut bad_utf = vec![OP_SEARCH, 5, 0, 0, 2, 0, 0, 0];
        bad_utf.extend_from_slice(&[0xff, 0xfe]);
        assert!(Request::decode(&bad_utf).is_err());
        let mut many = vec![STATUS_OK, 2];
        many.extend_from_slice(&0u64.to_le_bytes());
        many.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(Reply::decode(&many).is_err());
    }

    #[test]
    fn frames_round_trip_and_reject_oversize() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello").unwrap();
        write_frame(&mut buf, b"").unwrap();
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).unwrap().unwrap(), b"hello");
        assert_eq!(read_frame(&mut r).unwrap().unwrap(), b"");
        assert!(read_frame(&mut r).unwrap().is_none());

        let mut big = Vec::new();
        big.extend_from_slice(&((MAX_FRAME + 1) as u32).to_le_bytes());
        assert!(read_frame(&mut &big[..]).is_err());
        // Cut off mid-length and mid-body.
        assert!(read_frame(&mut &[1u8, 0][..]).is_err());
        assert!(read_frame(&mut &[5u8, 0, 0, 0, 1][..]).is_err());
        assert!(write_frame(&mut Vec::new(), &vec![0u8; MAX_FRAME + 1]).is_err());
    }
}
