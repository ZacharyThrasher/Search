//! A tantivy `Directory` that shrugs off transient Windows file errors.
//!
//! tantivy replaces `meta.json` and `managed.json` by writing a temporary file
//! and renaming it over the old one. On Windows that rename fails with "access
//! denied" whenever anyone else has the target open at that instant: the
//! popup reloading its reader, Defender scanning the file, the Search
//! indexer. One such miss fails a whole commit. This wrapper retries the
//! handful of operations involved for a short while before giving up.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::{
    Directory, DirectoryLock, FileHandle, FileSlice, Lock, MmapDirectory, WatchCallback, WatchHandle, WritePtr,
};

/// Tries for about this long before reporting the error.
const PATIENCE: Duration = Duration::from_secs(3);
const NAP: Duration = Duration::from_millis(20);

#[derive(Clone, Debug)]
pub struct PatientDirectory {
    inner: MmapDirectory,
}

impl PatientDirectory {
    pub fn open(path: impl AsRef<Path>) -> Result<PatientDirectory, tantivy::directory::error::OpenDirectoryError> {
        Ok(PatientDirectory { inner: MmapDirectory::open(path)? })
    }
}

/// Run `f` until it succeeds or the error is not worth retrying.
fn patiently<T, E>(mut f: impl FnMut() -> Result<T, E>, retry: impl Fn(&E) -> bool, budget: Duration) -> Result<T, E> {
    let start = std::time::Instant::now();
    loop {
        match f() {
            Err(e) if retry(&e) && start.elapsed() < budget => std::thread::sleep(NAP),
            other => return other,
        }
    }
}

fn transient(e: &io::Error) -> bool {
    // 5: access denied, 32: sharing violation, 33: lock violation. Windows
    // reports a rename over a busy file as any of them.
    matches!(e.raw_os_error(), Some(5 | 32 | 33)) || e.kind() == io::ErrorKind::PermissionDenied
}

impl Directory for PatientDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        patiently(
            || self.inner.get_file_handle(path),
            |e| matches!(e, OpenReadError::IoError { io_error, .. } if transient(io_error)),
            PATIENCE,
        )
    }

    fn open_read(&self, path: &Path) -> Result<FileSlice, OpenReadError> {
        patiently(
            || self.inner.open_read(path),
            |e| matches!(e, OpenReadError::IoError { io_error, .. } if transient(io_error)),
            PATIENCE,
        )
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        // A file a reader still has mapped cannot go; tantivy keeps it on its
        // list and tries again after the next commit, so do not wait long.
        patiently(
            || self.inner.delete(path),
            |e| matches!(e, DeleteError::IoError { io_error, .. } if transient(io_error)),
            Duration::from_millis(60),
        )
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        self.inner.exists(path)
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        patiently(
            || self.inner.open_write(path),
            |e| matches!(e, OpenWriteError::IoError { io_error, .. } if transient(io_error)),
            PATIENCE,
        )
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        patiently(
            || self.inner.atomic_read(path),
            |e| matches!(e, OpenReadError::IoError { io_error, .. } if transient(io_error)),
            PATIENCE,
        )
    }

    fn atomic_write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        patiently(|| self.inner.atomic_write(path, data), transient, PATIENCE)
    }

    fn sync_directory(&self) -> io::Result<()> {
        self.inner.sync_directory()
    }

    fn acquire_lock(&self, lock: &Lock) -> Result<DirectoryLock, LockError> {
        self.inner.acquire_lock(lock)
    }

    fn watch(&self, callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        self.inner.watch(callback)
    }
}
