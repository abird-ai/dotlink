//! Linux descriptor-rooted filesystem operations.
//!
//! Native tools never operate on a pathname after checking its policy:
//! every open uses openat2 beneath an already-open authorized directory,
//! refuses symlinks/magic links and nested mounts, and carries the returned
//! descriptor through IO. Existing symlink aliases may be canonicalized by
//! the policy before reaching this layer, but an alias swap cannot redirect
//! the final operation.
use std::{
    ffi::CString,
    fs::{self, File},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

const RESOLVE_NO_XDEV: u64 = 0x01;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;
const SAFE_RESOLVE: u64 = RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV;

fn openat2(dir: &OwnedFd, path: &Path, flags: i32, mode: u32, resolve: u64) -> io::Result<OwnedFd> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in filesystem path"))?;
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC) as u64,
        mode: mode as u64,
        resolve,
    };
    // SAFETY: all pointers refer to live stack/CString memory; the kernel
    // copies the fixed-size open_how struct before returning the owned FD.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dir.as_raw_fd(),
            path.as_ptr(),
            &how as *const OpenHow,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat2 returned a new descriptor owned by this process.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

fn mkdirat(dir: &OwnedFd, path: &Path) -> io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in directory name"))?;
    // SAFETY: dir is an open directory descriptor and path owns its NUL
    // terminated buffer for the duration of the call.
    let result = unsafe { libc::mkdirat(dir.as_raw_fd(), path.as_ptr(), 0o777) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    Ok(())
}

#[derive(Debug)]
struct Root {
    path: PathBuf,
    fd: OwnedFd,
    resolve: u64,
}

/// Immutable root capabilities. The same Root directory FDs are shared by
/// independent requests; none of the tools mutate a process-wide cwd.
#[derive(Debug)]
pub struct SecureFs {
    read: Vec<Root>,
    write: Vec<Root>,
}

impl SecureFs {
    pub fn new(read: &[PathBuf], write: &[PathBuf]) -> Result<Self> {
        fn open_roots(paths: &[PathBuf], already_open: &[Root]) -> Result<Vec<Root>> {
            let slash = fs::File::open("/")?;
            let slash: OwnedFd = slash.into();
            let mut roots = Vec::new();
            for path in paths {
                // A read+write grant may name the same root twice. Both
                // capabilities must refer to the same pinned inode even if
                // the host path is concurrently renamed during startup.
                if let Some(original) = already_open.iter().find(|root| root.path == *path) {
                    roots.push(Root {
                        path: path.clone(),
                        fd: original.fd.try_clone()?,
                        resolve: original.resolve,
                    });
                    continue;
                }
                let rel = path.strip_prefix("/")?;
                // These roots were canonicalized by the policy at startup.
                // Refuse a symlink added since then, rather than opening its target.
                let fd = openat2(
                    &slash,
                    if rel.as_os_str().is_empty() {
                        Path::new(".")
                    } else {
                        rel
                    },
                    libc::O_PATH | libc::O_DIRECTORY,
                    0,
                    RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
                )
                .with_context(|| {
                    format!("cannot securely open authorized root {}", path.display())
                })?;
                roots.push(Root {
                    path: path.clone(),
                    fd,
                    resolve: if path == Path::new("/") {
                        RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS
                    } else {
                        SAFE_RESOLVE
                    },
                });
            }
            roots.sort_by_key(|root| std::cmp::Reverse(root.path.components().count()));
            Ok(roots)
        }

        let read = open_roots(read, &[])?;
        let write = open_roots(write, &read)?;
        // Explicitly probe required kernel semantics; never silently downgrade
        // to pathname-based operations if openat2 is unavailable.
        if let Some(root) = read.first().or_else(|| write.first()) {
            openat2(
                &root.fd,
                Path::new("."),
                libc::O_PATH | libc::O_DIRECTORY,
                0,
                root.resolve,
            )
            .context("secure filesystem access requires Linux openat2 (5.6+)")?;
        }
        Ok(Self { read, write })
    }

    fn root<'a>(&'a self, target: &'a Path, write: bool) -> Result<(&'a Root, &'a Path)> {
        let roots = if write { &self.write } else { &self.read };
        roots
            .iter()
            .find_map(|root| target.strip_prefix(&root.path).ok().map(|rel| (root, rel)))
            .ok_or_else(|| {
                anyhow!(
                    "path has no authorized {} root: {}",
                    if write { "write" } else { "read" },
                    target.display()
                )
            })
    }

    fn open(&self, path: &Path, write: bool, flags: i32, mode: u32) -> Result<File> {
        let (root, rel) = self.root(path, write)?;
        let rel = if rel.as_os_str().is_empty() {
            Path::new(".")
        } else {
            rel
        };
        let fd = openat2(&root.fd, rel, flags, mode, root.resolve)
            .with_context(|| format!("refused unsafe or unavailable path {}", path.display()))?;
        Ok(File::from(fd))
    }

    pub fn open_read(&self, path: &Path) -> Result<File> {
        let file = self.open(path, false, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
        if !file.metadata()?.is_file() {
            bail!("path is not a regular file: {}", path.display());
        }
        Ok(file)
    }

    pub fn open_edit(&self, path: &Path) -> Result<File> {
        // Edit/patch requires *both* read and write authority. Using only the
        // writable root is insufficient when its grant is broader than the
        // readable root: a directory swap could otherwise select a different
        // file before this final open. Pin the deeper (narrower) grant root.
        let (read_root, _) = self.root(path, false)?;
        let (write_root, _) = self.root(path, true)?;
        let use_read_root =
            read_root.path.components().count() >= write_root.path.components().count();
        let file = self.open(path, !use_read_root, libc::O_RDWR | libc::O_NONBLOCK, 0)?;
        if !file.metadata()?.is_file() {
            bail!("path is not a regular file: {}", path.display());
        }
        Ok(file)
    }

    pub fn open_write(&self, path: &Path) -> Result<File> {
        self.open_write_mode(path, false)
    }

    /// Create-only write: refuses an existing path, including one that appeared
    /// between the policy check and this open (O_EXCL, or `create_new` elsewhere).
    pub fn open_create(&self, path: &Path) -> Result<File> {
        self.open_write_mode(path, true)
    }

    fn open_write_mode(&self, path: &Path, exclusive: bool) -> Result<File> {
        let (root, rel) = self.root(path, true)?;
        let Some(file_name) = rel.file_name() else {
            bail!("cannot replace an authorized root directory");
        };
        let mut current: OwnedFd = openat2(
            &root.fd,
            Path::new("."),
            libc::O_PATH | libc::O_DIRECTORY,
            0,
            root.resolve,
        )?;
        let parent = rel.parent().unwrap_or_else(|| Path::new(""));
        for component in parent.components() {
            let Component::Normal(name) = component else {
                bail!("unexpected component in relative path {}", path.display());
            };
            let child = Path::new(name);
            match openat2(
                &current,
                child,
                libc::O_PATH | libc::O_DIRECTORY,
                0,
                root.resolve,
            ) {
                Ok(fd) => current = fd,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    mkdirat(&current, child).with_context(|| {
                        format!("failed creating directory inside grant: {}", path.display())
                    })?;
                    current = openat2(
                        &current,
                        child,
                        libc::O_PATH | libc::O_DIRECTORY,
                        0,
                        root.resolve,
                    )?;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("unsafe parent in {}", path.display()));
                }
            }
        }
        let mut flags = libc::O_WRONLY | libc::O_CREAT | libc::O_NONBLOCK;
        if exclusive {
            flags |= libc::O_EXCL;
        }
        let fd = match openat2(&current, Path::new(file_name), flags, 0o666, root.resolve) {
            Ok(fd) => fd,
            Err(error) if exclusive && error.kind() == io::ErrorKind::AlreadyExists => {
                bail!("path already exists: {}", path.display())
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("refused unsafe write target {}", path.display()));
            }
        };
        let file = File::from(fd);
        if !file.metadata()?.is_file() {
            bail!("path is not a regular file: {}", path.display());
        }
        if !exclusive {
            file.set_len(0)?;
        }
        Ok(file)
    }

    pub fn open_directory(&self, path: &Path) -> Result<File> {
        self.open(path, false, libc::O_RDONLY | libc::O_DIRECTORY, 0)
    }

    /// Open an opted-in host service/config path by FD, refusing symlink
    /// replacement and verifying the actual pinned object type. Only the
    /// explicitly requested object is mounted; no parent directory authority
    /// is shared with the sandbox.
    pub fn open_host_mount(
        source: &Path,
        kind: crate::host_services::HostMountKind,
    ) -> Result<(File, PathBuf)> {
        use std::os::unix::fs::FileTypeExt;
        let target = std::fs::canonicalize(source)
            .with_context(|| format!("host integration disappeared: {}", source.display()))?;
        let slash: OwnedFd = File::open("/")?.into();
        let relative = target.strip_prefix("/")?;
        let fd = openat2(
            &slash,
            relative,
            libc::O_PATH | libc::O_NOFOLLOW,
            0,
            RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
        )
        .with_context(|| format!("unsafe host integration source {}", source.display()))?;
        let file = File::from(fd);
        let metadata = file.metadata()?;
        let valid = match kind {
            crate::host_services::HostMountKind::RegularFile => metadata.is_file(),
            crate::host_services::HostMountKind::Socket => metadata.file_type().is_socket(),
            crate::host_services::HostMountKind::Directory => metadata.is_dir(),
            crate::host_services::HostMountKind::CharacterDevice => {
                metadata.file_type().is_char_device()
            }
        };
        if !valid {
            bail!("host integration has wrong file type: {}", source.display());
        }
        // This is the path actually used for the descriptor-relative open.
        // Keep it alongside the FD so policy never re-resolves a mutable
        // symlink and accidentally checks a different target.
        Ok((file, target))
    }

    /// Pin an explicitly configured shell-only cache source to the directory
    /// validated at startup. This is not an MCP filesystem grant.
    pub fn open_canonical_directory(path: &Path) -> Result<File> {
        let slash: OwnedFd = File::open("/")?.into();
        let relative = path.strip_prefix("/")?;
        let relative = if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative
        };
        let fd = openat2(
            &slash,
            relative,
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
            RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
        )
        .with_context(|| format!("unsafe shell cache source {}", path.display()))?;
        Ok(File::from(fd))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn securely_denies_redirected_symlinks_and_writes_inside_grant() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), "secret").unwrap();
        let sandbox =
            SecureFs::new(std::slice::from_ref(&root), std::slice::from_ref(&root)).unwrap();

        // The original path may have been approved, but it must not be
        // reopened through an alias that changed to an external directory.
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        assert!(sandbox.open_read(&root.join("escape/secret")).is_err());
        assert!(sandbox.open_write(&root.join("escape/new")).is_err());
        assert!(!outside.join("new").exists());

        let mut file = sandbox.open_write(&root.join("nested/new.txt")).unwrap();
        use std::io::Write;
        file.write_all(b"hello").unwrap();
        assert_eq!(fs::read(root.join("nested/new.txt")).unwrap(), b"hello");
    }

    #[test]
    fn concurrent_symlink_swaps_cannot_redirect_opened_handles() {
        use std::io::{Read, Write};
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("granted");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        let selected = root.join("target");
        let secret = outside.join("secret");
        fs::write(&secret, "never-touch").unwrap();
        fs::write(&selected, "inside").unwrap();
        let safe = SecureFs::new(std::slice::from_ref(&root), std::slice::from_ref(&root)).unwrap();

        std::thread::scope(|scope| {
            let swapped = selected.clone();
            let secret = secret.clone();
            scope.spawn(move || {
                for _ in 0..250 {
                    let _ = fs::remove_file(&swapped);
                    let _ = symlink(&secret, &swapped);
                    std::thread::yield_now();
                    let _ = fs::remove_file(&swapped);
                    let _ = fs::write(&swapped, "inside");
                }
            });
            for _ in 0..250 {
                if let Ok(mut file) = safe.open_read(&selected) {
                    let mut text = String::new();
                    file.read_to_string(&mut text).unwrap();
                    assert_ne!(text, "never-touch");
                }
                if let Ok(mut file) = safe.open_write(&selected) {
                    file.write_all(b"local").unwrap();
                }
            }
        });
        assert_eq!(fs::read_to_string(secret).unwrap(), "never-touch");
    }

    #[test]
    fn edit_requires_both_pinned_read_and_write_grants() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let readable = root.join("readable");
        fs::create_dir_all(&readable).unwrap();
        fs::write(readable.join("visible"), b"visible").unwrap();
        let secure =
            SecureFs::new(std::slice::from_ref(&readable), std::slice::from_ref(&root)).unwrap();
        fs::rename(&readable, root.join("moved")).unwrap();
        fs::create_dir(&readable).unwrap();
        fs::write(readable.join("new-private"), b"private").unwrap();
        assert!(secure.open_edit(&readable.join("new-private")).is_err());
        assert_eq!(fs::read(readable.join("new-private")).unwrap(), b"private");

        // The narrower *write* root must be pinned when write authority is
        // more restrictive than the read grant.
        let writable = root.join("writable");
        fs::create_dir(&writable).unwrap();
        let secure =
            SecureFs::new(std::slice::from_ref(&root), std::slice::from_ref(&writable)).unwrap();
        fs::rename(&writable, root.join("moved-writable")).unwrap();
        fs::create_dir(&writable).unwrap();
        fs::write(writable.join("new-private"), b"private").unwrap();
        assert!(secure.open_edit(&writable.join("new-private")).is_err());
        assert_eq!(fs::read(writable.join("new-private")).unwrap(), b"private");
    }

    #[test]
    fn explicit_root_grant_allows_nested_mountpoints() {
        let secure = SecureFs::new(&[PathBuf::from("/")], &[]).unwrap();
        if Path::new("/proc").is_dir() {
            // /proc is a separate mount on ordinary Linux hosts. Explicit
            // root grants intentionally retain the ability to traverse it.
            assert!(secure.open_directory(Path::new("/proc")).is_ok());
        }
    }

    #[test]
    fn create_only_write_refuses_existing_and_symlinked_targets() {
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"secret").unwrap();
        let sandbox =
            SecureFs::new(std::slice::from_ref(&root), std::slice::from_ref(&root)).unwrap();

        fs::write(root.join("present"), b"present").unwrap();
        assert!(sandbox.open_create(&root.join("present")).is_err());
        assert_eq!(fs::read(root.join("present")).unwrap(), b"present");

        std::os::unix::fs::symlink(outside.join("secret"), root.join("alias")).unwrap();
        assert!(sandbox.open_create(&root.join("alias")).is_err());
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"secret");

        let mut file = sandbox.open_create(&root.join("nested/new.txt")).unwrap();
        file.write_all(b"hello").unwrap();
        assert_eq!(fs::read(root.join("nested/new.txt")).unwrap(), b"hello");
    }

    #[test]
    fn root_capability_cannot_operate_outside_itself() {
        let temp = tempfile::tempdir().unwrap();
        let one = temp.path().join("one");
        let two = temp.path().join("two");
        fs::create_dir(&one).unwrap();
        fs::create_dir(&two).unwrap();
        let secure = SecureFs::new(std::slice::from_ref(&one), std::slice::from_ref(&one)).unwrap();
        assert!(secure.open_write(&two.join("file")).is_err());
        assert!(!two.join("file").exists());
    }
}
