//! Linux descriptor-relative access: no symlink following, even for ancestor components.
use anyhow::{ensure, Context, Result};
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::Path,
};

pub struct Dir(pub File);
fn name(s: &str) -> Result<CString> {
    Ok(CString::new(s)?)
}
fn checked_fd(fd: i32) -> Result<File> {
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: successful open/openat returns a new owned fd.
    Ok(unsafe { File::from_raw_fd(fd) })
}
impl Dir {
    pub fn absolute(path: &Path) -> Result<Self> {
        crate::model::absolute_path(path)?;
        let root = name("/")?;
        // SAFETY: valid C string and flags; returned fd is owned.
        let mut dir = Self(checked_fd(unsafe {
            libc::open(
                root.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        })?);
        for component in path.components().skip(1) {
            let s = component
                .as_os_str()
                .to_str()
                .context("non UTF-8 directory")?;
            let c = name(s)?;
            // SAFETY: single component, no symlink following. O_PATH requires no
            // directory listing permission for ancestors in a restricted sandbox.
            dir = Self(checked_fd(unsafe {
                libc::openat(
                    dir.0.as_raw_fd(),
                    c.as_ptr(),
                    libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            })?);
        }
        let dot = name(".")?;
        // SAFETY: reopen only the final pinned directory for operations/fsync.
        Ok(Self(checked_fd(unsafe {
            libc::openat(
                dir.0.as_raw_fd(),
                dot.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?))
    }
    pub fn child(&self, component: &str) -> Result<Self> {
        crate::model::safe_id(component)?;
        self.child_raw(component)
    }
    fn child_raw(&self, component: &str) -> Result<Self> {
        let c = name(component)?;
        // SAFETY: self fd is a directory; component is a single C string.
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        Ok(Self(
            checked_fd(fd).with_context(|| format!("open directory {component}"))?,
        ))
    }
    pub fn mkdir(&self, component: &str) -> Result<Self> {
        crate::model::safe_id(component)?;
        let c = name(component)?;
        // SAFETY: valid fd/name, mode has no special bits.
        let result = unsafe { libc::mkdirat(self.0.as_raw_fd(), c.as_ptr(), 0o700) };
        ensure!(
            result == 0,
            "cannot create directory {component}: {}",
            std::io::Error::last_os_error()
        );
        self.child(component)
    }
    pub fn open_file(&self, component: &str, writable: bool) -> Result<File> {
        crate::model::relative_file(component).or_else(|e| {
            if component == "release.json" {
                Ok(())
            } else {
                Err(e)
            }
        })?;
        ensure!(!component.contains('/'), "expected filename");
        let c = name(component)?;
        let access = if writable {
            libc::O_RDWR
        } else {
            libc::O_RDONLY
        };
        // NONBLOCK prevents a malicious FIFO from blocking before metadata validation.
        // SAFETY: valid fd/name and open flags.
        let file = checked_fd(unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                c.as_ptr(),
                access | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        })?;
        let m = file.metadata()?;
        ensure!(
            m.is_file() && m.nlink() == 1,
            "expected ordinary single-link file: {component}"
        );
        Ok(file)
    }
    pub fn relative_file(&self, path: &str) -> Result<File> {
        crate::model::relative_file(path)?;
        let mut parts: Vec<_> = path.split('/').collect();
        let leaf = parts.pop().context("empty path")?;
        let mut dir = Self(self.0.try_clone()?);
        for part in parts {
            dir = dir.child(part)?;
        }
        dir.open_file(leaf, false)
    }
    pub fn create(&self, component: &str) -> Result<File> {
        crate::model::safe_id(component)?;
        let c = name(component)?;
        // SAFETY: new file only, no symlinks followed; valid dir fd/name.
        checked_fd(unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        })
    }
    pub fn write_new(&self, component: &str, bytes: &[u8], mode: u32) -> Result<()> {
        let mut file = self.create(component)?;
        file.write_all(bytes)?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        Ok(())
    }
    pub fn sync(&self) -> Result<()> {
        Ok(self.0.sync_all()?)
    }
    pub fn seal(&self) -> Result<()> {
        self.0
            .set_permissions(std::fs::Permissions::from_mode(0o555))?;
        self.sync()
    }
    pub fn publish(&self, old: &str, new: &str) -> Result<()> {
        let old = name(old)?;
        let new = name(new)?;
        // SAFETY: both paths are validated single components and dir fd remains owned.
        let result = unsafe {
            libc::renameat2(
                self.0.as_raw_fd(),
                old.as_ptr(),
                self.0.as_raw_fd(),
                new.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        ensure!(
            result == 0,
            "publish refused: {}",
            std::io::Error::last_os_error()
        );
        self.sync()
    }
}

pub fn read_json_bytes(mut file: File) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut file)
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4 * 1024 * 1024, "JSON exceeds 4MiB");
    Ok(bytes)
}

/// Caller owns the directory and parent hierarchy. Same-UID attackers are out of scope.
pub fn private_store(dir: &Dir) -> Result<()> {
    let meta = dir.0.metadata()?;
    // SAFETY: geteuid has no preconditions.
    ensure!(
        meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
        "store must be owned by caller and private (0700)"
    );
    ensure!(
        read_json_bytes(dir.open_file("store-format", false)?)? == b"echo-deploy-candidates-v1\n",
        "not an echo-deploy candidate store"
    );
    Ok(())
}

pub struct Lock(File);
impl Lock {
    pub fn acquire(dir: &Dir) -> Result<Self> {
        let file = dir.open_file("store-lock", true)?;
        // SAFETY: owned regular file fd, flock is released when File closes.
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "candidate store busy: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(file))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: valid fd; closing also releases the lock.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
