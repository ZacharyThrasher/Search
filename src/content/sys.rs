//! Windows odds and ends the indexer needs: user idleness, battery state,
//! background priority, a job object that keeps the extractor children on a
//! leash, and the single-instance and stop events.

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, SetInformationJobObject,
        TerminateJobObject,
    };
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::System::Threading::{
        CreateEventW, CreateMutexW, GetCurrentProcess, IDLE_PRIORITY_CLASS, MEMORY_PRIORITY_INFORMATION,
        MEMORY_PRIORITY_VERY_LOW, OpenEventW, ProcessMemoryPriority, SetEvent, SetPriorityClass,
        SetProcessInformation, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    const EVENT_MODIFY_STATE: u32 = 0x0002;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const ERROR_ALREADY_EXISTS: u32 = 183;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// Seconds since the last keyboard or mouse input in this session.
    pub fn idle_seconds() -> u64 {
        // SAFETY: LASTINPUTINFO is a plain struct with its size set.
        unsafe {
            let mut li = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
            if GetLastInputInfo(&mut li) == 0 {
                return u64::MAX; // cannot tell: treat as away
            }
            // Both are 32-bit millisecond tick counts that wrap together.
            GetTickCount().wrapping_sub(li.dwTime) as u64 / 1000
        }
    }

    /// Running on battery (a desktop, or an unknown state, counts as plugged in).
    pub fn on_battery() -> bool {
        // SAFETY: out-struct of the right type.
        unsafe {
            let mut s: SYSTEM_POWER_STATUS = std::mem::zeroed();
            GetSystemPowerStatus(&mut s) != 0 && s.ACLineStatus == 0
        }
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtSetInformationProcess(process: HANDLE, class: u32, info: *mut core::ffi::c_void, len: u32) -> i32;
    }

    /// Lowest CPU, I/O and memory priority for the whole process.
    ///
    /// `PROCESS_MODE_BACKGROUND_BEGIN` alone lowers the I/O and memory
    /// priority but leaves the CPU class at Normal (measured), so the three
    /// are set one by one: idle CPU class, very low I/O priority (the same
    /// value background mode uses), very low memory priority.
    pub fn enter_background_mode() {
        // SAFETY: plain calls on the current process; the info structs live across the calls.
        unsafe {
            let me = GetCurrentProcess();
            SetPriorityClass(me, IDLE_PRIORITY_CLASS);
            let mut io: u32 = 0; // IoPriorityVeryLow
            NtSetInformationProcess(me, 33 /* ProcessIoPriority */, &mut io as *mut u32 as *mut _, 4);
            let mem = MEMORY_PRIORITY_INFORMATION { MemoryPriority: MEMORY_PRIORITY_VERY_LOW };
            SetProcessInformation(
                me,
                ProcessMemoryPriority,
                &mem as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
            );
        }
    }

    /// Owns a Windows job: children assigned to it die with the parent and
    /// cannot use more than `memory_limit` bytes each.
    pub struct Job(HANDLE);

    // SAFETY: a job handle is a kernel object reference.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn new(memory_limit: usize) -> Option<Job> {
            // SAFETY: standard job-object setup; the handle is closed on failure and in Drop.
            unsafe {
                let h = CreateJobObjectW(null(), null());
                if h.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
                info.ProcessMemoryLimit = memory_limit;
                let ok = SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    CloseHandle(h);
                    return None;
                }
                Some(Job(h))
            }
        }

        pub fn assign(&self, child: &Child) -> bool {
            // SAFETY: both handles are valid for the call.
            unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) != 0 }
        }

        /// Kill every process in the job now.
        pub fn terminate(&self) {
            // SAFETY: our own job handle.
            unsafe { TerminateJobObject(self.0, 1) };
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closing the handle we own (kills the children, by design).
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Hold a named mutex for the life of the process. `false`: another
    /// process already holds it.
    pub fn single_instance(name: &str) -> bool {
        // SAFETY: the handle is intentionally leaked; it lives as long as the process.
        unsafe {
            let h = CreateMutexW(null(), 0, wide(name).as_ptr());
            !(h.is_null() || GetLastError() == ERROR_ALREADY_EXISTS)
        }
    }

    /// A named manual-reset event; `wait` is true once someone signalled it.
    pub struct StopEvent(HANDLE);

    // SAFETY: event handles may be used from any thread.
    unsafe impl Send for StopEvent {}
    unsafe impl Sync for StopEvent {}

    impl StopEvent {
        pub fn create(name: &str) -> Option<StopEvent> {
            // SAFETY: valid name; the handle is closed in Drop.
            let h = unsafe { CreateEventW(null(), 1, 0, wide(name).as_ptr()) };
            (!h.is_null()).then_some(StopEvent(h))
        }

        pub fn is_set(&self) -> bool {
            // SAFETY: waiting on our own handle with a zero timeout.
            unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
        }
    }

    impl Drop for StopEvent {
        fn drop(&mut self) {
            // SAFETY: closing our handle.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Signal a running indexer to quit. `false` if none is running.
    pub fn signal_stop(name: &str) -> bool {
        // SAFETY: open, set, close.
        unsafe {
            let h = OpenEventW(EVENT_MODIFY_STATE | SYNCHRONIZE, 0, wide(name).as_ptr());
            if h.is_null() {
                return false;
            }
            let ok = SetEvent(h) != 0;
            CloseHandle(h);
            ok
        }
    }

    /// Windows attribute bits that say the file's data is not on this disk
    /// (OneDrive "online only" and similar): reading it would download it.
    pub fn is_cloud_placeholder(attrs: u32) -> bool {
        const OFFLINE: u32 = 0x1000;
        const RECALL_ON_OPEN: u32 = 0x4_0000;
        const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
        attrs & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0
    }

    pub fn is_reparse_point(attrs: u32) -> bool {
        attrs & 0x400 != 0
    }

    #[allow(dead_code)]
    fn unused() {
        let _ = null_mut::<u8>();
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn idle_seconds() -> u64 {
        u64::MAX
    }
    pub fn on_battery() -> bool {
        false
    }
    pub fn enter_background_mode() {}
    pub struct Job;
    impl Job {
        pub fn new(_: usize) -> Option<Job> {
            None
        }
        pub fn assign(&self, _: &std::process::Child) -> bool {
            false
        }
        pub fn terminate(&self) {}
    }
    pub fn single_instance(_: &str) -> bool {
        true
    }
    pub struct StopEvent;
    impl StopEvent {
        pub fn create(_: &str) -> Option<StopEvent> {
            None
        }
        pub fn is_set(&self) -> bool {
            false
        }
    }
    pub fn signal_stop(_: &str) -> bool {
        false
    }
    pub fn is_cloud_placeholder(_: u32) -> bool {
        false
    }
    pub fn is_reparse_point(_: u32) -> bool {
        false
    }
}

pub use imp::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_return_something_sensible() {
        let _ = idle_seconds();
        let _ = on_battery();
        assert!(!is_cloud_placeholder(0x20));
        #[cfg(windows)]
        {
            assert!(is_cloud_placeholder(0x40_0000));
            assert!(is_reparse_point(0x400));
            // A job accepts a child and kills it when closed.
            let job = Job::new(512 << 20).expect("job");
            let mut child = std::process::Command::new("cmd").args(["/c", "ping -n 30 127.0.0.1 >nul"]).spawn().unwrap();
            assert!(job.assign(&child));
            drop(job);
            std::thread::sleep(std::time::Duration::from_millis(500));
            assert!(child.try_wait().unwrap().is_some(), "child outlived its job");
        }
    }

    #[cfg(windows)]
    #[test]
    fn stop_event_round_trip() {
        let name = format!("Local\\SearchTestStop{}", std::process::id());
        let ev = StopEvent::create(&name).unwrap();
        assert!(!ev.is_set());
        assert!(signal_stop(&name));
        assert!(ev.is_set());
        assert!(!signal_stop("Local\\SearchTestNoSuchEvent"));
    }
}
