//! Staged writes: the platform-neutral policy, and the path backend.
//!
//! Every write is staged beside its target and installed in one step. Two rules
//! live here because both backends must agree on them:
//!
//! - a temporary is created with a mode **no broader than the target's**, so
//!   replacing a `0600` file never exposes its bytes to another user while the
//!   replacement is being written;
//! - a temporary is named `.dotlink-<pid>-<16 hex>.tmp`, which the shared walk
//!   hides so a listing or a search never reports a half-installed file.
//!
//! Linux stages beneath a pinned parent descriptor (`secure_fs::StagedWrite`);
//! other platforms stage beside the target path and install atomically here.

use std::ffi::{OsStr, OsString};

/// Visible name prefix of a staging temporary.
const STAGING_PREFIX: &str = ".dotlink-";
const STAGING_SUFFIX: &str = ".tmp";
/// Fixed width of the random word in a staging name.
const STAGING_RANDOM_HEX: usize = 16;

/// Mode for a staged replacement: never broader than the target's own mode. A
/// target that does not exist yet keeps ordinary create semantics (0666 minus
/// the process umask), because there is no earlier mode to protect.
pub(crate) fn staging_mode(existing: Option<u32>) -> u32 {
    existing.map_or(0o666, |mode| mode & 0o7777)
}

/// Fixed-length staging name. A target at the filesystem's `NAME_MAX` limit
/// would otherwise produce an over-long temporary; pid plus a random word keeps
/// concurrent writers apart.
pub(crate) fn staging_name() -> OsString {
    format!(
        "{STAGING_PREFIX}{}-{:016x}{STAGING_SUFFIX}",
        std::process::id(),
        fastrand::u64(..)
    )
    .into()
}

/// True for a name this process (or another dotlink) stages under. The walk
/// hides these: they carry in-flight bytes that are not yet a file the caller
/// can see, and reporting one would make a listing depend on unrelated writes.
pub(crate) fn is_staging_temporary(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(rest) = name.strip_prefix(STAGING_PREFIX) else {
        return false;
    };
    let Some(rest) = rest.strip_suffix(STAGING_SUFFIX) else {
        return false;
    };
    let Some((pid, random)) = rest.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && random.len() == STAGING_RANDOM_HEX
        && random.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Pathname backend for platforms without descriptor-rooted IO.
#[cfg(any(test, not(target_os = "linux")))]
mod path_backend {
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result, anyhow, bail};
    use tokio::{
        fs,
        io::{AsyncReadExt, AsyncWriteExt},
    };

    use super::{staging_mode, staging_name};

    /// Windows replacement must never widen a file's DACL, even briefly.
    /// Read the original before staging, apply its exact protected DACL to the
    /// still-empty temporary, then use ReplaceFileW to preserve attributes and
    /// security as part of the final install. All errors fail closed.
    #[cfg(windows)]
    fn file_dacl(path: &Path) -> Result<Vec<u8>> {
        use std::{io, os::windows::ffi::OsStrExt};
        use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, GetFileSecurityW};

        let wide = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let mut required = 0_u32;
        // SAFETY: wide is NUL-terminated; zero-length buffer queries required size.
        unsafe {
            GetFileSecurityW(
                wide.as_ptr(),
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                0,
                &mut required,
            );
        }
        if required == 0 || required > 1024 * 1024 {
            return Err(io::Error::last_os_error()).context("cannot read target Windows DACL");
        }
        let mut descriptor = vec![0_u8; required as usize];
        // SAFETY: descriptor has the kernel-reported length and outlives the call.
        let ok = unsafe {
            GetFileSecurityW(
                wide.as_ptr(),
                DACL_SECURITY_INFORMATION,
                descriptor.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error()).context("cannot read target Windows DACL");
        }
        Ok(descriptor)
    }

    #[cfg(windows)]
    fn protect_temporary(path: &Path, dacl: &[u8]) -> Result<()> {
        use std::{io, os::windows::ffi::OsStrExt};
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SetFileSecurityW,
        };
        let wide = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        // SAFETY: descriptor is a self-relative SECURITY_DESCRIPTOR from
        // GetFileSecurityW; the file is empty until this operation succeeds.
        let ok = unsafe {
            SetFileSecurityW(
                wide.as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                dacl.as_ptr().cast_mut().cast(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error()).context("cannot protect Windows temporary");
        }
        Ok(())
    }

    #[cfg(windows)]
    fn install_new_name(staged: &Path, target: &Path) -> Result<()> {
        use std::{io, os::windows::ffi::OsStrExt};
        use windows_sys::Win32::Storage::FileSystem::MoveFileW;
        let from = staged
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let to = target
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        // SAFETY: paths are NUL-terminated and valid until the call returns.
        // Unlike replacement moves, MoveFileW refuses an occupied target.
        if unsafe { MoveFileW(from.as_ptr(), to.as_ptr()) } == 0 {
            return Err(io::Error::last_os_error()).with_context(|| {
                format!(
                    "cannot create {} without replacing an existing file",
                    target.display()
                )
            });
        }
        Ok(())
    }

    #[cfg(windows)]
    fn replace_preserving_acl(existing: &Path, staged: &Path, backup: &Path) -> Result<()> {
        use std::{io, os::windows::ffi::OsStrExt};
        use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
        let target = existing
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let replacement = staged
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let backup = backup
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        // SAFETY: all paths are NUL-terminated and remain live for this call.
        // Zero flags deliberately forbid ignoring ACL or metadata merge errors.
        let ok = unsafe {
            ReplaceFileW(
                target.as_ptr(),
                replacement.as_ptr(),
                backup.as_ptr(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error()).with_context(|| {
                format!(
                    "cannot replace {} while preserving Windows file security",
                    existing.display()
                )
            });
        }
        Ok(())
    }

    /// `ReplaceFileW` can rename the original to `backup` and then fail to
    /// install `staged`. Recover by moving the backup back without overwriting
    /// a name that another actor may have created. If anything is uncertain,
    /// preserve the staged and backup artifacts for manual recovery.
    #[cfg(windows)]
    pub(super) fn recover_failed_windows_replace(target: &Path, backup: &Path) -> bool {
        let target_exists = match std::fs::symlink_metadata(target) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => return false,
        };
        let backup_exists = match std::fs::symlink_metadata(backup) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => return false,
        };
        match (target_exists, backup_exists) {
            // Original never moved. It is safe to discard our staged copy.
            (true, false) => true,
            // Windows moved the original to the backup but failed to install
            // the replacement. Restore without overwriting a concurrent file.
            (false, true) => install_new_name(backup, target).is_ok(),
            // A backup and a live target, or neither: preserve all artifacts.
            _ => false,
        }
    }

    /// Own the temporary from the instant the file is created, *including*
    /// across async suspension. Cancellation drops this guard and unlinks the
    /// temporary; successful staging transfers its name to `StagedWrite`.
    /// The file handle is owned here so it closes before cleanup is attempted.
    #[derive(Debug)]
    pub(super) struct PendingTemporary {
        path: PathBuf,
        file: Option<fs::File>,
        armed: bool,
    }

    impl PendingTemporary {
        pub(super) fn new(path: PathBuf, file: fs::File) -> Self {
            Self {
                path,
                file: Some(file),
                armed: true,
            }
        }

        pub(super) fn file(&mut self) -> &mut fs::File {
            self.file
                .as_mut()
                .expect("pending temporary owns its open file")
        }

        fn finish(mut self) -> PathBuf {
            // Closing the file precedes transfer of cleanup ownership.
            drop(self.file.take());
            self.armed = false;
            std::mem::take(&mut self.path)
        }
    }

    impl Drop for PendingTemporary {
        fn drop(&mut self) {
            drop(self.file.take());
            if self.armed
                && let Err(error) = std::fs::remove_file(&self.path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(%error, path = %self.path.display(),
                    "could not remove cancelled staging temporary");
            }
        }
    }

    /// A replacement that is written and flushed but not yet visible. Dropping
    /// it removes the temporary; `commit` installs it over the target or, for a
    /// create-only write, links it into a free name.
    #[derive(Debug)]
    pub(crate) struct StagedWrite {
        target: PathBuf,
        temporary: PathBuf,
        create_only: bool,
        /// Whether the name existed when staging began.
        existed: bool,
        /// Length and SHA-256 of the bytes the caller planned against. Updates
        /// re-check this immediately before the install.
        expected: Option<(u64, [u8; 32])>,
        committed: bool,
        #[cfg(windows)]
        preserve_staged_on_failure: bool,
    }

    /// Stage `bytes` beside `path`. An existing target must be a regular file;
    /// its mode is preserved from the first syscall, never widened mid-write.
    pub(crate) async fn stage(
        path: &Path,
        bytes: &[u8],
        create_only: bool,
        expected: Option<(u64, [u8; 32])>,
    ) -> Result<StagedWrite> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("cannot replace a root directory: {}", path.display()))?;
        fs::create_dir_all(parent).await?;
        let existing_permissions = match fs::symlink_metadata(path).await {
            Ok(_) if create_only => bail!("path already exists: {}", path.display()),
            Ok(metadata) if metadata.is_file() => Some(metadata.permissions()),
            Ok(_) => bail!("path is not a regular file: {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };

        #[cfg(windows)]
        let dacl = existing_permissions
            .as_ref()
            .map(|_| file_dacl(path))
            .transpose()?;
        let temporary = parent.join(staging_name());
        // The mode a staged file starts at: the target's own mode, or the usual
        // create mode when there is no earlier mode to protect. Windows has no
        // POSIX mode, so it inherits the parent's ACLs and ignores the value.
        #[cfg(unix)]
        let staged_mode = {
            use std::os::unix::fs::PermissionsExt as _;
            staging_mode(
                existing_permissions
                    .as_ref()
                    .map(|permissions| permissions.mode() & 0o7777),
            )
        };
        #[cfg(not(unix))]
        let staged_mode = staging_mode(None);
        // Create synchronously: `tokio::fs::OpenOptions::open().await` can
        // create a file in a blocking worker after its awaiting future was
        // cancelled, before an RAII guard could own the resulting name.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(staged_mode);
        }
        #[cfg(not(unix))]
        let _ = staged_mode;
        let existed = existing_permissions.is_some();
        let file = fs::File::from_std(options.open(&temporary)?);
        let mut pending = PendingTemporary::new(temporary, file);
        // Every subsequent suspension point is covered by PendingTemporary.
        #[cfg(windows)]
        if let Some(dacl) = dacl.as_deref() {
            // Even a transient is guarded: no bytes are written until the
            // original restrictive ACL has been applied successfully.
            protect_temporary(&pending.path, dacl)?;
        }
        pending.file().write_all(bytes).await?;
        if let Some(permissions) = existing_permissions {
            pending.file().set_permissions(permissions).await?;
        }
        pending.file().sync_all().await?;
        let temporary = pending.finish();
        Ok(StagedWrite {
            target: path.to_path_buf(),
            temporary,
            create_only,
            existed,
            expected,
            committed: false,
            #[cfg(windows)]
            preserve_staged_on_failure: false,
        })
    }

    impl StagedWrite {
        /// Make the staged bytes visible. The target is re-checked (type and,
        /// for an update, the expected bytes) immediately before the install.
        pub(crate) async fn commit(mut self) -> Result<()> {
            if !self.create_only {
                match fs::symlink_metadata(&self.target).await {
                    Ok(metadata) if metadata.is_file() => {}
                    Ok(_) => bail!("path is not a regular file: {}", self.target.display()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                if let Some((len, digest)) = self.expected {
                    use sha2::{Digest as _, Sha256};

                    let mut file = fs::File::open(&self.target).await.with_context(|| {
                        format!("{} changed before commit", self.target.display())
                    })?;
                    if !file.metadata().await?.is_file() {
                        bail!("{} changed before commit", self.target.display());
                    }
                    // Stream the re-check: the target may be far larger than a
                    // write payload, and it must not be buffered to hash it.
                    let mut hasher = Sha256::new();
                    let mut buffer = vec![0_u8; 64 * 1024];
                    let mut total = 0_u64;
                    loop {
                        let read = file.read(&mut buffer).await?;
                        if read == 0 {
                            break;
                        }
                        total = total.saturating_add(read as u64);
                        if total > len {
                            break;
                        }
                        hasher.update(&buffer[..read]);
                    }
                    let found: [u8; 32] = hasher.finalize().into();
                    if total != len || found != digest {
                        bail!("{} changed before commit", self.target.display());
                    }
                }
            }
            let outcome: Result<()> =
                if self.create_only {
                    // A no-overwrite install refuses a name introduced concurrently.
                    #[cfg(windows)]
                    {
                        install_new_name(&self.temporary, &self.target)
                    }
                    #[cfg(not(windows))]
                    {
                        fs::hard_link(&self.temporary, &self.target)
                            .await
                            .map_err(Into::into)
                    }
                } else if !self.existed {
                    // Preserve normal create-or-replace support on macOS filesystems
                    // without hard links. Windows uses an atomic no-overwrite move.
                    #[cfg(windows)]
                    {
                        install_new_name(&self.temporary, &self.target)
                    }
                    #[cfg(not(windows))]
                    {
                        fs::rename(&self.temporary, &self.target)
                            .await
                            .map_err(Into::into)
                    }
                } else {
                    #[cfg(windows)]
                    {
                        // A named backup is essential: without it, Windows can
                        // remove the original name even when ReplaceFileW fails.
                        let backup = self
                            .target
                            .parent()
                            .ok_or_else(|| anyhow!("replacement target has no parent"))?
                            .join(staging_name());
                        self.preserve_staged_on_failure = true;
                        match replace_preserving_acl(&self.target, &self.temporary, &backup) {
                            Ok(()) => {
                                self.preserve_staged_on_failure = false;
                                // The original survives in the backup after a
                                // successful commit; cleanup cannot undo the commit.
                                if let Err(error) = std::fs::remove_file(&backup) {
                                    tracing::warn!(%error, path = %backup.display(),
                                    "obsolete Windows replacement backup could not be removed");
                                }
                                Ok(())
                            }
                            Err(error) => {
                                self.preserve_staged_on_failure =
                                    !recover_failed_windows_replace(&self.target, &backup);
                                Err(error.context(format!(
                                "Windows replacement recovery: target={}, backup={}, staged={}{}",
                                self.target.display(), backup.display(), self.temporary.display(),
                                if self.preserve_staged_on_failure {
                                    "; preserved recovery files: inspect before cleaning up"
                                } else {
                                    "; original target is present"
                                }
                            )))
                            }
                        }
                    }
                    #[cfg(not(windows))]
                    {
                        fs::rename(&self.temporary, &self.target)
                            .await
                            .map_err(Into::into)
                    }
                };
            match outcome {
                Ok(()) => {
                    self.committed = true;
                    // Only Unix's create-only hard link leaves a second name.
                    if self.create_only && !cfg!(windows) {
                        let _ = fs::remove_file(&self.temporary).await;
                    }
                    Ok(())
                }
                Err(error) => Err(error),
            }
        }
    }

    impl Drop for StagedWrite {
        fn drop(&mut self) {
            if !self.committed {
                #[cfg(windows)]
                if self.preserve_staged_on_failure {
                    return;
                }
                let _ = std::fs::remove_file(&self.temporary);
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) use path_backend::{StagedWrite, stage as stage_path};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_mode_never_widens_an_existing_target() {
        // The regression: a 0600 file used to be staged at 0666 & ~umask, so a
        // second user could read the replacement while it was being written.
        assert_eq!(staging_mode(Some(0o600)), 0o600);
        assert_eq!(staging_mode(Some(0o640)), 0o640);
        assert_eq!(staging_mode(Some(0o755)), 0o755);
        assert_eq!(staging_mode(Some(0o4755)), 0o4755);
        // A new file has no earlier mode to protect and keeps create semantics.
        assert_eq!(staging_mode(None), 0o666);
    }

    #[test]
    fn staging_names_are_recognized_and_nothing_else_is() {
        let name = staging_name();
        assert!(is_staging_temporary(&name), "{name:?}");
        assert!(is_staging_temporary(OsStr::new(
            ".dotlink-1234-0123456789abcdef.tmp"
        )));
        for other in [
            ".dotlink-1234-0123456789abcde.tmp",     // random word too short
            ".dotlink-1234-0123456789abcdefghi.tmp", // too long
            ".dotlink-1234-0123456789abcdeZ.tmp",    // not hex
            ".dotlink-x-0123456789abcdef.tmp",       // pid not numeric
            ".dotlink-0123456789abcdef.tmp",         // no pid
            "dotlink-1234-0123456789abcdef.tmp",     // no leading dot
            ".dotlink-1234-0123456789abcdef",        // no suffix
            "notes.txt",
        ] {
            assert!(
                !is_staging_temporary(OsStr::new(other)),
                "{other} must not be treated as a staging temporary"
            );
        }
    }

    #[test]
    fn staging_names_do_not_collide() {
        let names = (0..512)
            .map(|_| staging_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(names.len(), 512);
        assert!(names.iter().all(|name| name.len() <= 64), "fixed length");
    }
}

/// Exercises the path backend where it is the real implementation (macOS and
/// Windows; on Linux `secure_fs` stages instead). The body is platform-neutral
/// on purpose, so it also compiles on Windows where the Unix permission API does
/// not exist; the mode assertions are Unix-only.
#[cfg(test)]
mod path_backend_tests {
    use super::path_backend::stage as stage_path;
    use super::*;

    #[tokio::test]
    async fn cancelling_a_pending_write_removes_its_temporary() {
        use tokio::sync::oneshot;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(staging_name());
        let expected_path = path.clone();
        let (ready, started) = oneshot::channel();
        let task = tokio::spawn(async move {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            let mut pending =
                super::path_backend::PendingTemporary::new(path, tokio::fs::File::from_std(file));
            ready.send(()).unwrap();
            // The task is cancelled with the file and cleanup guard alive.
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            pending.file().sync_all().await.unwrap();
        });
        started.await.unwrap();
        assert!(expected_path.exists());
        task.abort();
        let _ = task.await;
        assert!(!expected_path.exists(), "cancelled stage left a temporary");
    }

    #[cfg(windows)]
    #[test]
    fn replacement_failure_recovers_original_or_keeps_backup() {
        use crate::staging::path_backend::recover_failed_windows_replace;
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("original.txt");
        let backup = root.path().join("backup.txt");
        let staged = root.path().join("staged.txt");
        std::fs::write(&target, b"original").unwrap();
        std::fs::write(&staged, b"replacement").unwrap();
        // The documented ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 state:
        // original moved to backup, staged remains under its own name.
        std::fs::rename(&target, &backup).unwrap();
        assert!(recover_failed_windows_replace(&target, &backup));
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        assert_eq!(std::fs::read(&staged).unwrap(), b"replacement");
        // An ambiguous state retains all copies rather than deleting the
        // remaining replacement or backup.
        std::fs::write(&backup, b"original-backup").unwrap();
        assert!(!recover_failed_windows_replace(&target, &backup));
        assert_eq!(std::fs::read(&backup).unwrap(), b"original-backup");
    }

    #[tokio::test]
    async fn path_backend_is_atomic_private_and_create_only() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();

        // Create-only: absent until commit, then the whole payload.
        let staged = stage_path(&root.join("nested/new.txt"), b"first", true, None)
            .await
            .unwrap();
        assert!(!root.join("nested/new.txt").exists(), "absent until commit");
        staged.commit().await.unwrap();
        assert_eq!(
            std::fs::read(root.join("nested/new.txt")).unwrap(),
            b"first"
        );

        // Update: the temporary already carries the target's own mode.
        let target = root.join("secret");
        std::fs::write(&target, b"old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let expected = (3, crate::mcp::sha256(b"old"));
        let staged = stage_path(&target, b"new", false, Some(expected))
            .await
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for entry in std::fs::read_dir(&root).unwrap().flatten() {
                if is_staging_temporary(&entry.file_name()) {
                    assert_eq!(
                        entry.metadata().unwrap().permissions().mode() & 0o777,
                        0o600,
                        "the in-flight temporary must not be broader than the target"
                    );
                }
            }
        }
        staged.commit().await.unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        // A stale expectation refuses the install and leaves the bytes alone.
        let staged = stage_path(&target, b"third", false, Some((3, [0_u8; 32])))
            .await
            .unwrap();
        assert!(staged.commit().await.is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"new");

        // Create-only refuses an existing target; a directory is never replaced.
        assert!(stage_path(&target, b"x", true, None).await.is_err());
        std::fs::create_dir(root.join("dir")).unwrap();
        assert!(
            stage_path(&root.join("dir"), b"x", false, None)
                .await
                .is_err()
        );
    }
}
