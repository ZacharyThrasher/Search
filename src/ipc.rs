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

use crate::events::{
    Event, EventKind, Filter, MAX_EVENTS_PER_REPLY, MAX_FILTER_ITEM_BYTES, MAX_FILTER_ITEMS, PollState,
};

/// Bumped when the format changes; the popup refuses a helper that disagrees.
pub const PROTOCOL_VERSION: u16 = 2;

/// Largest frame either side will accept (a full page of long paths is ~30 KB).
pub const MAX_FRAME: usize = 1 << 20;
pub const MAX_QUERY_BYTES: usize = 1024;
pub const MAX_LIMIT: usize = 500;

const OP_STATUS: u8 = 1;
const OP_SEARCH: u8 = 2;
const OP_SAVE: u8 = 3;
const OP_SHUTDOWN: u8 = 4;
const OP_SUBSCRIBE: u8 = 5;
const OP_EVENTS: u8 = 6;
const OP_LIST_FILES: u8 = 7;

/// Longest the helper will hold an `Events` request open waiting for news.
pub const MAX_EVENT_WAIT_MS: u32 = 10_000;
/// Soft cap on the bytes of paths in one `Files` reply (frames stop at `MAX_FRAME`).
pub const FILES_PAGE_BYTES: usize = 600 * 1024;
pub const MAX_FILES_PER_PAGE: usize = 10_000;

/// Where a file listing stands: the next entry of a volume's index. The
/// generation changes when that index is replaced (compaction, rescan), which
/// renumbers entries; a listing cannot continue across that.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListCursor {
    pub volume: u16,
    pub entry: u32,
    pub generation: u64,
}

pub const STATUS_OK: u8 = 0;
/// The index is still loading; the body is a progress line.
pub const STATUS_LOADING: u8 = 1;
/// The request was refused; the body is a message.
pub const STATUS_ERROR: u8 = 2;

/// `query` cut to at most `MAX_QUERY_BYTES` bytes at a character boundary, so
/// a very long paste is searched by its start instead of being refused.
pub fn clamp_query(query: &str) -> &str {
    if query.len() <= MAX_QUERY_BYTES {
        return query;
    }
    let mut end = MAX_QUERY_BYTES;
    while !query.is_char_boundary(end) {
        end -= 1;
    }
    &query[..end]
}

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
    /// Start recording change events for paths `Filter` allows (replaces any
    /// earlier filter). Answered with `Subscribed`.
    Subscribe(Filter),
    /// Events from sequence number `next` on, waiting up to `wait_ms` if none.
    Events {
        epoch: u64,
        next: u64,
        max: u16,
        wait_ms: u32,
    },
    /// A page of the files the filter allows, in index order.
    ListFiles {
        cursor: ListCursor,
        filter: Filter,
    },
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
    Subscribed {
        epoch: u64,
        next: u64,
    },
    Events {
        state: PollState,
        epoch: u64,
        next: u64,
        events: Vec<Event>,
    },
    /// One page of a listing. `next` is `None` after the last page. `restart`
    /// means the index changed under the listing: start over (no paths).
    Files {
        restart: bool,
        next: Option<ListCursor>,
        paths: Vec<String>,
    },
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

fn put_filter(out: &mut Vec<u8>, f: &Filter) {
    for list in [&f.exts, &f.exclude_components, &f.exclude_prefixes, &f.exclude_fragments, &f.include_prefixes] {
        out.extend_from_slice(&(list.len() as u16).to_le_bytes());
        for item in list {
            put_str(out, item);
        }
    }
    out.push(f.skip_dot_dirs as u8);
}

impl Cur<'_> {
    fn list(&mut self, max: usize) -> Result<Vec<String>, BadMessage> {
        let n = self.u16()? as usize;
        if n > max {
            return bad("too many filter items");
        }
        (0..n).map(|_| self.str(MAX_FILTER_ITEM_BYTES)).collect()
    }

    fn filter(&mut self) -> Result<Filter, BadMessage> {
        Ok(Filter {
            exts: self.list(MAX_FILTER_ITEMS[0])?,
            exclude_components: self.list(MAX_FILTER_ITEMS[1])?,
            exclude_prefixes: self.list(MAX_FILTER_ITEMS[2])?,
            exclude_fragments: self.list(MAX_FILTER_ITEMS[3])?,
            include_prefixes: self.list(MAX_FILTER_ITEMS[4])?,
            skip_dot_dirs: self.u8()? != 0,
        })
    }

    fn cursor(&mut self) -> Result<ListCursor, BadMessage> {
        Ok(ListCursor {
            volume: self.u16()?,
            entry: self.u32()?,
            generation: self.u64()?,
        })
    }
}

fn put_cursor(out: &mut Vec<u8>, c: &ListCursor) {
    out.extend_from_slice(&c.volume.to_le_bytes());
    out.extend_from_slice(&c.entry.to_le_bytes());
    out.extend_from_slice(&c.generation.to_le_bytes());
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
            Request::Subscribe(f) => {
                out.push(OP_SUBSCRIBE);
                put_filter(&mut out, f);
            }
            Request::Events { epoch, next, max, wait_ms } => {
                out.push(OP_EVENTS);
                out.extend_from_slice(&epoch.to_le_bytes());
                out.extend_from_slice(&next.to_le_bytes());
                out.extend_from_slice(&max.to_le_bytes());
                out.extend_from_slice(&wait_ms.to_le_bytes());
            }
            Request::ListFiles { cursor, filter } => {
                out.push(OP_LIST_FILES);
                put_cursor(&mut out, cursor);
                put_filter(&mut out, filter);
            }
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
            OP_SUBSCRIBE => Request::Subscribe(c.filter()?),
            OP_EVENTS => Request::Events {
                epoch: c.u64()?,
                next: c.u64()?,
                max: c.u16()?,
                wait_ms: c.u32()?,
            },
            OP_LIST_FILES => Request::ListFiles {
                cursor: c.cursor()?,
                filter: c.filter()?,
            },
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
            Reply::Subscribed { epoch, next } => {
                out.push(STATUS_OK);
                out.push(4);
                out.extend_from_slice(&epoch.to_le_bytes());
                out.extend_from_slice(&next.to_le_bytes());
            }
            Reply::Events { state, epoch, next, events } => {
                out.push(STATUS_OK);
                out.push(5);
                out.push(match state {
                    PollState::Ok => 0,
                    PollState::Lost => 1,
                    PollState::NewEpoch => 2,
                    PollState::NotSubscribed => 3,
                });
                out.extend_from_slice(&epoch.to_le_bytes());
                out.extend_from_slice(&next.to_le_bytes());
                out.extend_from_slice(&(events.len() as u32).to_le_bytes());
                for e in events {
                    out.extend_from_slice(&e.seq.to_le_bytes());
                    let kind = match e.kind {
                        EventKind::Changed => 0u8,
                        EventKind::Deleted => 1,
                        EventKind::Renamed => 2,
                    };
                    out.push(kind | (e.is_dir as u8) << 4);
                    put_str(&mut out, &e.path);
                    put_str(&mut out, &e.old_path);
                }
            }
            Reply::Files { restart, next, paths } => {
                out.push(STATUS_OK);
                out.push(6);
                out.push(*restart as u8 | (next.is_some() as u8) << 1);
                put_cursor(&mut out, &next.unwrap_or_default());
                out.extend_from_slice(&(paths.len() as u32).to_le_bytes());
                for p in paths {
                    put_str(&mut out, p);
                }
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
                4 => Reply::Subscribed {
                    epoch: c.u64()?,
                    next: c.u64()?,
                },
                5 => {
                    let state = match c.u8()? {
                        0 => PollState::Ok,
                        1 => PollState::Lost,
                        2 => PollState::NewEpoch,
                        3 => PollState::NotSubscribed,
                        _ => return bad("unknown poll state"),
                    };
                    let epoch = c.u64()?;
                    let next = c.u64()?;
                    let n = c.u32()? as usize;
                    if n > MAX_EVENTS_PER_REPLY {
                        return bad("too many events");
                    }
                    let mut events = Vec::with_capacity(n);
                    for _ in 0..n {
                        let seq = c.u64()?;
                        let b = c.u8()?;
                        let kind = match b & 0x0F {
                            0 => EventKind::Changed,
                            1 => EventKind::Deleted,
                            2 => EventKind::Renamed,
                            _ => return bad("unknown event kind"),
                        };
                        events.push(Event {
                            seq,
                            kind,
                            is_dir: b & 0x10 != 0,
                            path: c.str(MAX_FRAME)?,
                            old_path: c.str(MAX_FRAME)?,
                        });
                    }
                    Reply::Events { state, epoch, next, events }
                }
                6 => {
                    let flags = c.u8()?;
                    let cur = c.cursor()?;
                    let n = c.u32()? as usize;
                    if n > MAX_FILES_PER_PAGE {
                        return bad("too many paths");
                    }
                    let mut paths = Vec::with_capacity(n.min(1024));
                    for _ in 0..n {
                        paths.push(c.str(MAX_FRAME)?);
                    }
                    Reply::Files {
                        restart: flags & 1 != 0,
                        next: (flags & 2 != 0).then_some(cur),
                        paths,
                    }
                }
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
            Request::Subscribe(Filter {
                exts: vec!["txt".into(), "md".into()],
                exclude_components: vec!["node_modules".into()],
                exclude_prefixes: vec!["\\windows\\".into()],
                exclude_fragments: vec![],
                include_prefixes: vec!["c:\\users\\".into()],
                skip_dot_dirs: true,
            }),
            Request::Events { epoch: 7, next: 99, max: 500, wait_ms: 1500 },
            Request::ListFiles {
                cursor: ListCursor { volume: 1, entry: 123_456, generation: 9 },
                filter: Filter::default(),
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
            Reply::Subscribed { epoch: 5, next: 1 },
            Reply::Events {
                state: PollState::Ok,
                epoch: 5,
                next: 12,
                events: vec![
                    Event { seq: 10, kind: EventKind::Changed, is_dir: false, path: "C:\\a.txt".into(), old_path: String::new() },
                    Event { seq: 11, kind: EventKind::Renamed, is_dir: true, path: "C:\\b".into(), old_path: "C:\\a".into() },
                ],
            },
            Reply::Events { state: PollState::Lost, epoch: 1, next: 2, events: vec![] },
            Reply::Files {
                restart: false,
                next: Some(ListCursor { volume: 0, entry: 77, generation: 3 }),
                paths: vec!["C:\\x.txt".into(), "D:\\y.md".into()],
            },
            Reply::Files { restart: true, next: None, paths: vec![] },
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
    fn oversized_filters_are_refused() {
        let too_many = Filter {
            exclude_fragments: (0..MAX_FILTER_ITEMS[3] + 1).map(|i| format!("frag{i}")).collect(),
            ..Default::default()
        };
        assert!(Request::decode(&Request::Subscribe(too_many).encode()).is_err());
        let too_long = Filter { exts: vec!["x".repeat(MAX_FILTER_ITEM_BYTES + 1)], ..Default::default() };
        assert!(Request::decode(&Request::Subscribe(too_long).encode()).is_err());
        // Right at the limits is fine.
        let at_limit = Filter {
            exclude_fragments: (0..MAX_FILTER_ITEMS[3]).map(|i| format!("frag{i}")).collect(),
            exts: vec!["x".repeat(MAX_FILTER_ITEM_BYTES)],
            ..Default::default()
        };
        assert!(Request::decode(&Request::Subscribe(at_limit).encode()).is_ok());
    }

    #[test]
    fn long_queries_are_clamped_to_what_the_helper_accepts() {
        assert_eq!(clamp_query("abc"), "abc");
        let long = "a".repeat(MAX_QUERY_BYTES + 50);
        assert_eq!(clamp_query(&long).len(), MAX_QUERY_BYTES);
        // Never splits a multi-byte character (here 3 bytes each, 1024 = 341 * 3 + 1).
        let wide = "\u{20AC}".repeat(400);
        let cut = clamp_query(&wide);
        assert_eq!(cut.len(), 341 * 3);
        let r = Request::Search { query: cut.into(), limit: 5, kind: KindFilter::Any };
        assert_eq!(Request::decode(&r.encode()).unwrap(), r);
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
