//! Cross-process serialization of private control-plane state.
//! The stable lock file is independent of the atomically replaced payload.
//! Dropping the file releases the lock on all normal/cancellation paths.

use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::Path,
    time::{Duration, Instant},
};

pub(crate) fn acquire(path: &Path, timeout: Duration) -> Result<File> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("lock path has no parent"))?;
    fs::create_dir_all(parent)?;
    crate::setup::set_private_dir_permissions(parent)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("cannot open state lock {}", path.display()))?;
    crate::setup::set_private_file_permissions(path)?;
    let deadline = Instant::now() + timeout;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(fs::TryLockError::WouldBlock) => {
                bail!("state is busy: {}; retry", path.display());
            }
            Err(fs::TryLockError::Error(error)) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(fs::TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("cannot lock {}", path.display()));
            }
        }
    }
}
