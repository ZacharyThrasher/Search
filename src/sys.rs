//! Small process-level probes for the CLI report: memory use and elevation.

#[derive(Debug, Clone, Copy)]
pub struct ProcMem {
    pub working_set: usize,
    pub peak_working_set: usize,
}

#[cfg(windows)]
pub fn process_memory() -> Option<ProcMem> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: counters is a zeroed, correctly sized out-struct.
    unsafe {
        let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) == 0 {
            return None;
        }
        Some(ProcMem {
            working_set: c.WorkingSetSize,
            peak_working_set: c.PeakWorkingSetSize,
        })
    }
}

#[cfg(not(windows))]
pub fn process_memory() -> Option<ProcMem> {
    None
}

/// Whether the current process token is elevated (raw volume access needs it).
#[cfg(windows)]
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // SAFETY: standard token query; handle closed on all paths.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut ret = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elev as *mut _ as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        );
        CloseHandle(token);
        ok != 0 && elev.TokenIsElevated != 0
    }
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    false
}

/// For a program built with the Windows (no console) subsystem: if it was
/// started from a terminal, write to that terminal. Returns whether it did.
#[cfg(windows)]
pub fn attach_parent_console() -> bool {
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    // SAFETY: plain console calls; the CONOUT$ handle is intentionally kept for the life of the process.
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return false;
        }
        let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
        let h = CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null_mut(),
            OPEN_EXISTING,
            0,
            null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            return false;
        }
        SetStdHandle(STD_OUTPUT_HANDLE, h);
        SetStdHandle(STD_ERROR_HANDLE, h);
        true
    }
}

#[cfg(not(windows))]
pub fn attach_parent_console() -> bool {
    false
}

/// Never let Windows pop up "The system cannot find the drive specified" (or
/// similar) boxes for a stray path: failed opens just return errors.
#[cfg(windows)]
pub fn quiet_errors() {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX, SetErrorMode,
    };
    // SAFETY: process-wide flag, no pointers.
    unsafe { SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOOPENFILEERRORBOX | SEM_NOGPFAULTERRORBOX) };
}

#[cfg(not(windows))]
pub fn quiet_errors() {}

/// Write a line to `path` if the process dies from a native fault (access
/// violation, stack overflow...), which Rust's panic hook never sees. The
/// error-dialog suppression of `quiet_errors` would otherwise make it silent.
#[cfg(windows)]
pub fn log_native_crashes(path: std::path::PathBuf) {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::Diagnostics::Debug::{EXCEPTION_POINTERS, SetUnhandledExceptionFilter};
    static PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
    let _ = PATH.set(path);
    unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes valid exception pointers.
        let (code, addr) = unsafe {
            let rec = (*info).ExceptionRecord;
            ((*rec).ExceptionCode, (*rec).ExceptionAddress as usize)
        };
        if let Some(p) = PATH.get()
            && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p)
        {
            use std::io::Write;
            let module = faulting_module(addr);
            let _ = writeln!(f, "crash: exception {code:#x} at {addr:#x} in {module}");
        }
        1 // EXCEPTION_EXECUTE_HANDLER: end the process
    }
    // SAFETY: installs a process-wide filter once at startup.
    unsafe { SetUnhandledExceptionFilter(Some(filter)) };
}

#[cfg(windows)]
fn faulting_module(addr: usize) -> String {
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW,
        GetModuleHandleExW,
    };
    // SAFETY: address lookup into the loaded-module list; the buffer length is passed.
    unsafe {
        let mut h = std::ptr::null_mut();
        let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        if GetModuleHandleExW(flags, addr as *const u16, &mut h) == 0 {
            return "?".into();
        }
        let mut buf = [0u16; 260];
        let n = GetModuleFileNameW(h, buf.as_mut_ptr(), buf.len() as u32) as usize;
        format!("{} (+{:#x})", String::from_utf16_lossy(&buf[..n]), addr - h as usize)
    }
}

#[cfg(not(windows))]
pub fn log_native_crashes(_: std::path::PathBuf) {}
