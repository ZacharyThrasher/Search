//! Named-pipe transport for the helper protocol (Windows).
//!
//! The pipe is per user: its name carries the user's SID and its DACL grants
//! access to that SID only, so other accounts cannot connect, and remote
//! machines are refused outright. The server is the elevated helper; the
//! client is the unelevated popup (or any other tool running as the same user).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::ipc::{self, Reply, Request};

const MAX_CONNECTIONS: usize = 16;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// The current user's SID as a string such as `S-1-5-21-...`.
pub fn current_user_sid() -> io::Result<String> {
    // SAFETY: standard token query; buffers are sized by the first call and the
    // token handle and the string returned by ConvertSidToStringSidW are freed.
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut need = 0u32;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut need);
        let mut buf = vec![0u8; need as usize];
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), need, &mut need);
        CloseHandle(token);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s: *mut u16 = null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut s) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut n = 0;
        while *s.add(n) != 0 {
            n += 1;
        }
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
        LocalFree(s.cast());
        Ok(out)
    }
}

/// `\\.\pipe\SearchHelper-<SID>`, or `\\.\pipe\SearchHelper-<tag>-<SID>` for tests.
pub fn pipe_name(tag: Option<&str>) -> io::Result<String> {
    let sid = current_user_sid()?;
    Ok(match tag {
        Some(t) => format!(r"\\.\pipe\SearchHelper-{t}-{sid}"),
        None => format!(r"\\.\pipe\SearchHelper-{sid}"),
    })
}

/// Create one pipe instance. `first` demands that none exists yet, which is
/// how a second helper notices the first.
fn create_instance(name: &str, sid: &str, first: bool) -> io::Result<HANDLE> {
    // Full access for this user only; a medium-integrity label so an elevated
    // server stays reachable from the user's normal (unelevated) processes but
    // not from low-integrity (sandboxed) ones.
    let sddl = wide(&format!("D:P(A;;GA;;;{sid})S:(ML;;NW;;;ME)"));
    // SAFETY: the descriptor returned by the conversion is freed after the
    // pipe is created (the pipe keeps its own copy); all pointers are valid for the calls.
    unsafe {
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut())
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let mut open_mode = PIPE_ACCESS_DUPLEX;
        if first {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let h = CreateNamedPipeW(
            wide(name).as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            64 * 1024,
            64 * 1024,
            0,
            &sa,
        );
        let err = io::Error::last_os_error();
        LocalFree(sd.cast());
        if h == INVALID_HANDLE_VALUE {
            return Err(err);
        }
        Ok(h)
    }
}

/// True when the error means "another server already owns this pipe name".
pub fn is_already_running(e: &io::Error) -> bool {
    e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
}

/// Answer requests on `name` until the process exits. Returns an error only if
/// the first instance cannot be created (see `is_already_running`). `ready`
/// is called once the pipe exists, before the first client is accepted.
pub fn serve(
    name: &str,
    handler: Arc<dyn Fn(Request) -> Reply + Send + Sync>,
    ready: impl FnOnce(),
) -> io::Result<()> {
    let sid = current_user_sid()?;
    let mut h = create_instance(name, &sid, true)?;
    ready();
    let live = Arc::new(AtomicUsize::new(0));
    loop {
        // SAFETY: `h` is a valid pipe handle owned by this loop until handed to a connection.
        let connected = unsafe { ConnectNamedPipe(h, null_mut()) != 0 || GetLastError() == ERROR_PIPE_CONNECTED };
        if connected {
            if live.load(Ordering::Acquire) >= MAX_CONNECTIONS {
                // SAFETY: closing the handle we own.
                unsafe { CloseHandle(h) };
            } else {
                // SAFETY: ownership of the handle moves into the File.
                let file = unsafe { File::from_raw_handle(h as RawHandle) };
                live.fetch_add(1, Ordering::AcqRel);
                let (handler, live2) = (handler.clone(), live.clone());
                let spawned = thread::Builder::new().name("pipe-conn".into()).spawn(move || {
                    // Released even if the handler panics, so a bad request cannot
                    // use up the connection slots one by one.
                    let _slot = Slot(live2);
                    let _ = connection(file, &*handler);
                });
                if spawned.is_err() {
                    live.fetch_sub(1, Ordering::AcqRel);
                }
            }
        } else {
            // SAFETY: closing the handle we own.
            unsafe { CloseHandle(h) };
        }
        h = create_instance(name, &sid, false)?;
    }
}

/// One of the `MAX_CONNECTIONS` slots; frees itself on drop.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn connection(mut f: File, handler: &(dyn Fn(Request) -> Reply + Send + Sync)) -> io::Result<()> {
    while let Some(frame) = ipc::read_frame(&mut f)? {
        let reply = match Request::decode(&frame) {
            Ok(req) => handler(req),
            Err(e) => Reply::Error(e.to_string()),
        };
        ipc::write_frame(&mut f, &reply.encode())?;
    }
    Ok(())
}

/// A connection to the helper. One request at a time.
pub struct Client {
    f: File,
}

impl Client {
    /// Connect, waiting up to `wait` for a busy or not-yet-created pipe.
    pub fn connect(name: &str, wait: Duration) -> io::Result<Client> {
        let t = Instant::now();
        loop {
            // SECURITY_IDENTIFICATION: the server may learn who we are but can
            // not impersonate us.
            let r = OpenOptions::new()
                .read(true)
                .write(true)
                .security_qos_flags(SECURITY_IDENTIFICATION)
                .open(name);
            match r {
                Ok(f) => return Ok(Client { f }),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    let left = wait.saturating_sub(t.elapsed());
                    if left.is_zero() {
                        return Err(e);
                    }
                    // SAFETY: valid NUL-terminated name.
                    unsafe { WaitNamedPipeW(wide(name).as_ptr(), left.as_millis().min(u32::MAX as u128) as u32) };
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound && t.elapsed() < wait => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn request(&mut self, req: &Request) -> io::Result<Reply> {
        ipc::write_frame(&mut self.f, &req.encode())?;
        let frame = ipc::read_frame(&mut self.f)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "helper closed the connection"))?;
        Reply::decode(&frame).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{HitMsg, KindFilter};

    fn start(tag: &str) -> String {
        let name = pipe_name(Some(tag)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let n = name.clone();
        thread::spawn(move || {
            let h: Arc<dyn Fn(Request) -> Reply + Send + Sync> = Arc::new(|r| match r {
                Request::Search { query, .. } => Reply::Hits {
                    total: 1,
                    hits: vec![HitMsg { path: format!("C:\\{query}"), is_dir: false }],
                },
                Request::Status => Reply::Loading("warming".into()),
                _ => Reply::Done,
            });
            let _ = serve(&n, h, || tx.send(()).unwrap());
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        name
    }

    #[test]
    fn client_and_server_talk_over_a_real_pipe() {
        let name = start("test-talk");
        let mut c = Client::connect(&name, Duration::from_secs(2)).unwrap();
        for q in ["a", "résumé", "long".repeat(100).as_str()] {
            let r = c
                .request(&Request::Search { query: q.into(), limit: 5, kind: KindFilter::Any })
                .unwrap();
            assert_eq!(
                r,
                Reply::Hits { total: 1, hits: vec![HitMsg { path: format!("C:\\{q}"), is_dir: false }] }
            );
        }
        assert_eq!(c.request(&Request::Status).unwrap(), Reply::Loading("warming".into()));
        // A second client works while the first stays connected.
        let mut c2 = Client::connect(&name, Duration::from_secs(2)).unwrap();
        assert_eq!(c2.request(&Request::Save).unwrap(), Reply::Done);
        assert_eq!(c.request(&Request::Save).unwrap(), Reply::Done);
    }

    #[test]
    fn a_second_server_on_the_same_name_is_refused() {
        let name = start("test-twice");
        let h: Arc<dyn Fn(Request) -> Reply + Send + Sync> = Arc::new(|_| Reply::Done);
        let e = serve(&name, h, || {}).unwrap_err();
        assert!(is_already_running(&e), "{e}");
    }

    #[test]
    fn garbage_gets_an_error_reply_not_a_crash() {
        use std::io::Write;
        let name = start("test-garbage");
        let mut f = OpenOptions::new().read(true).write(true).open(&name).unwrap();
        ipc::write_frame(&mut f, &[200, 1, 2]).unwrap();
        let reply = Reply::decode(&ipc::read_frame(&mut f).unwrap().unwrap()).unwrap();
        assert!(matches!(reply, Reply::Error(_)));
        // An oversize length prefix closes the connection instead of allocating.
        f.write_all(&u32::MAX.to_le_bytes()).unwrap();
        assert!(ipc::read_frame(&mut f).map(|o| o.is_none()).unwrap_or(true));
        // The server is still serving others.
        let mut c = Client::connect(&name, Duration::from_secs(2)).unwrap();
        assert_eq!(c.request(&Request::Save).unwrap(), Reply::Done);
    }
}
