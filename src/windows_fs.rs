//! Windows filesystem authority: pin every pathname directory and open
//! file handles without following the final reparse point. A canonical policy
//! path is *not* a stable capability until these handles are held.
//
//! Directory handles deliberately exclude FILE_SHARE_DELETE. A concurrent
//! rename of any ancestor cannot redirect a checked path while an operation
//! is using it. Reparse points are refused even if introduced after the
//! original policy canonicalization.
use std::{
    io,
    os::windows::{
        ffi::OsStrExt as _,
        io::{FromRawHandle, OwnedHandle},
    },
    path::{Component, Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use windows_sys::Win32::{
    Foundation::{GENERIC_READ, GENERIC_WRITE},
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
        OPEN_EXISTING,
    },
};

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn raw_open(path: &Path, access: u32, share: u32, flags: u32) -> io::Result<OwnedHandle> {
    let name = wide(path);
    // SAFETY: NUL-terminated Windows path; handles are wrapped exactly once.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            share,
            std::ptr::null(),
            OPEN_EXISTING,
            flags | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful CreateFileW returns an owned handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn checked_attributes(handle: &OwnedHandle, directory: bool) -> Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: valid live handle and writable fixed-size output.
    let result = unsafe { GetFileInformationByHandle(handle.as_raw_handle(), info.as_mut_ptr()) };
    if result == 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: API returned success, so all output fields are initialized.
    let attributes = unsafe { info.assume_init() }.dwFileAttributes;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!("Windows reparse point is not permitted for native filesystem access");
    }
    let is_directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if is_directory != directory {
        bail!("Windows filesystem object type changed during authorization");
    }
    Ok(())
}

/// An ancestor and the requested directory are kept open with no delete
/// sharing, preventing renames or junction swaps across subsequent pathname
/// operations. For a new path, absent parents are created under held ancestors.
#[derive(Debug)]
pub(crate) struct PinnedDirectory {
    _handles: Vec<OwnedHandle>,
}

impl PinnedDirectory {
    pub(crate) fn new(path: &Path, create_missing: bool) -> Result<Self> {
        if !path.is_absolute() {
            bail!("Windows native filesystem path must be absolute");
        }
        let mut current = PathBuf::new();
        let mut handles = Vec::new();
        for part in path.components() {
            current.push(part.as_os_str());
            if !matches!(part, Component::Normal(_)) {
                continue;
            }
            let open = || {
                raw_open(
                    &current,
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    FILE_FLAG_BACKUP_SEMANTICS,
                )
            };
            let handle = match open() {
                Ok(handle) => handle,
                Err(error) if create_missing && error.kind() == io::ErrorKind::NotFound => {
                    match std::fs::create_dir(&current) {
                        Ok(()) => (),
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!("cannot create pinned directory {}", current.display())
                            });
                        }
                    };
                    open().with_context(|| {
                        format!(
                            "cannot securely open created directory {}",
                            current.display()
                        )
                    })?
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("cannot pin Windows directory {}", current.display())
                    });
                }
            };
            checked_attributes(&handle, true)?;
            handles.push(handle);
        }
        Ok(Self { _handles: handles })
    }

    pub(crate) fn parent(path: &Path, create_missing: bool) -> Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("file path has no parent directory"))?;
        Self::new(parent, create_missing)
    }
}

/// The final file is also opened without following a late symlink. Once open,
/// the handle refers to the same inode even if unrelated software renames it.
pub(crate) fn open_file(path: &Path, write: bool) -> Result<std::fs::File> {
    let _parents = PinnedDirectory::parent(path, false)?;
    let file = raw_open(
        path,
        if write {
            GENERIC_READ | GENERIC_WRITE
        } else {
            GENERIC_READ
        },
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        0,
    )?;
    checked_attributes(&file, false)?;
    Ok(std::fs::File::from(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reparse_escape_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let grant = temp.path().join("grant");
        let outsider = temp.path().join("outside");
        std::fs::create_dir(&grant).unwrap();
        std::fs::create_dir(&outsider).unwrap();
        std::fs::write(outsider.join("secret"), b"never leak").unwrap();
        let alias = grant.join("jump");
        if std::os::windows::fs::symlink_dir(&outsider, &alias).is_ok() {
            assert!(open_file(&alias.join("secret"), false).is_err());
            assert!(PinnedDirectory::new(&alias, false).is_err());
        }
    }
}
