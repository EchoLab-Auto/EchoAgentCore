//! Cross-process serialization for explicit transaction stores.
use crate::safe_fs::Dir;
use anyhow::{ensure, Result};
use std::{
    fs::File,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::Path,
};
pub struct StateLock(File);
impl StateLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        let dir = Dir::absolute(path)?;
        let metadata = dir.0.metadata()?;
        // SAFETY: geteuid has no preconditions.
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "state directory must be caller-owned and private"
        );
        let file = match dir.open_file("transaction-lock", true) {
            Ok(f) => f,
            Err(_) => {
                // Atomic create then reopen; a concurrent creator can win.
                let _ = dir.create("transaction-lock");
                dir.open_file("transaction-lock", true)?
            }
        };
        // SAFETY: owned regular file fd, advisory exclusive lock.
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "deployment transaction store busy"
        );
        Ok(Self(file))
    }
}
impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = self.0.sync_all();
    }
}
