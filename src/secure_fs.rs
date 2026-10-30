//! Descriptor-rooted Unix filesystem operations (Linux and macOS).
//!
//! Native tools never operate on a pathname after checking its policy:
//! opens stay beneath already-open authorized directories,
//! refuses symlinks/magic links and nested mounts, and carries the returned
//! descriptor through IO. Existing symlink aliases may be canonicalized by
//! the policy before reaching this layer, but an alias swap cannot redirect
//! the final operation.
use std::{
    ffi::CString,
    fs::{self, File},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, fs::PermissionsExt},
    },
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

#[cfg(target_os = "linux")]
const PATH_ONLY: i32 = libc::O_PATH;
#[cfg(target_os = "macos")]
const PATH_ONLY: i32 = libc::O_RDONLY;

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

/// macOS has no Linux openat2; walk one component at a time from the
/// pinned descriptor. O_NOFOLLOW at each open rejects swapped symlinks.
#[cfg(any(target_os = "macos", test))]
fn openat_beneath(
    dir: &OwnedFd,
    path: &Path,
    flags: i32,
    mode: u32,
    resolve: u64,
) -> io::Result<OwnedFd> {
    use std::os::unix::{ffi::OsStrExt as _, fs::MetadataExt as _};
    fn open_part(dir: &OwnedFd, part: &Path, flags: i32, mode: u32) -> io::Result<OwnedFd> {
        let name = CString::new(part.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in filesystem path"))?;
        // SAFETY: valid descriptor and NUL-terminated path.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat returns a new owned descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
    let mut parts = path.components().peekable();
    let mut cursor = dir.try_clone()?;
    if parts.peek().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty file path",
        ));
    }
    let mut current_device = File::from(cursor.try_clone()?).metadata()?.dev();
    while let Some(part) = parts.next() {
        let name = match part {
            Component::Normal(name) => Path::new(name),
            Component::CurDir => Path::new("."),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "path escapes pinned directory",
                ));
            }
        };
        let final_component = parts.peek().is_none();
        let next = open_part(
            &cursor,
            name,
            if final_component {
                flags
            } else {
                libc::O_RDONLY | libc::O_DIRECTORY
            },
            if final_component { mode } else { 0 },
        )?;
        let metadata = File::from(next.try_clone()?).metadata()?;
        if (resolve & RESOLVE_NO_XDEV != 0) && metadata.dev() != current_device {
            return Err(io::Error::from_raw_os_error(libc::EXDEV));
        }
        if !final_component && !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "non-directory path component",
            ));
        }
        current_device = metadata.dev();
        cursor = next;
    }
    Ok(cursor)
}

#[cfg(target_os = "macos")]
fn openat2(dir: &OwnedFd, path: &Path, flags: i32, mode: u32, resolve: u64) -> io::Result<OwnedFd> {
    openat_beneath(dir, path, flags, mode, resolve)
}

/// Inspect a single child under the same constraints as descriptor-rooted
/// opens. An `AT_SYMLINK_NOFOLLOW` stat alone is insufficient: a mountpoint
/// must not leak metadata where the grant forbids RESOLVE_NO_XDEV.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChildMetadata {
    pub(crate) kind: &'static str,
    pub(crate) len: u64,
    pub(crate) modified: Option<std::time::SystemTime>,
}

pub(crate) fn child_metadata(
    parent: &OwnedFd,
    name: &std::ffi::OsStr,
    resolve: u64,
) -> io::Result<ChildMetadata> {
    if !matches!(
        Path::new(name).components().next(),
        Some(Component::Normal(_))
    ) || Path::new(name).components().count() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected one child name",
        ));
    }

    #[cfg(target_os = "linux")]
    {
        // O_PATH|O_NOFOLLOW allows an FD referring to the symlink itself.
        // Kernel-enforced RESOLVE_NO_XDEV also detects bind mounts on the
        // same device, unlike comparing st_dev after an ordinary fstatat.
        let fd = open_child(parent, name, PATH_ONLY | libc::O_NOFOLLOW, resolve)?;
        let metadata = File::from(fd).metadata()?;
        let kind = if metadata.file_type().is_symlink() {
            "symlink"
        } else if metadata.is_dir() {
            "dir"
        } else if metadata.is_file() {
            "file"
        } else {
            "other"
        };
        Ok(ChildMetadata {
            kind,
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
    #[cfg(target_os = "macos")]
    {
        use std::time::{Duration, UNIX_EPOCH};
        // Darwin cannot O_PATH-open a symlink; preserve no-follow reporting
        // with fstatat and apply the same filesystem-device check used by the
        // component-wise macOS openat backend.
        let name = CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in child name"))?;
        let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: pinned directory FD, NUL-terminated child, valid output.
        if unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                info.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: success initialized the stat structure.
        let info = unsafe { info.assume_init() };
        if resolve & RESOLVE_NO_XDEV != 0 {
            let mut parent_info = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: parent is a live pinned FD and output is initialized on success.
            if unsafe { libc::fstat(parent.as_raw_fd(), parent_info.as_mut_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fstat succeeded.
            if info.st_dev != unsafe { parent_info.assume_init() }.st_dev {
                return Err(io::Error::from_raw_os_error(libc::EXDEV));
            }
        }
        let kind = match info.st_mode & libc::S_IFMT {
            libc::S_IFDIR => "dir",
            libc::S_IFREG => "file",
            libc::S_IFLNK => "symlink",
            _ => "other",
        };
        let seconds = info.st_mtime;
        let nanos = info.st_mtime_nsec;
        let modified = u32::try_from(nanos)
            .ok()
            .filter(|nanos| *nanos < 1_000_000_000)
            .and_then(|nanos| {
                let duration = Duration::from_secs(seconds.unsigned_abs());
                let base = if seconds < 0 {
                    UNIX_EPOCH.checked_sub(duration)
                } else {
                    UNIX_EPOCH.checked_add(duration)
                };
                base.and_then(|time| time.checked_add(Duration::from_nanos(u64::from(nanos))))
            });
        Ok(ChildMetadata {
            kind,
            len: info.st_size.max(0) as u64,
            modified,
        })
    }
}

/// Flags every child open must carry, whatever mask the caller supplies. The
/// mask is an implementation detail of the pinned root; keeping the security
/// boundary inside this function means a future caller cannot relax it by
/// passing a stale or hand-built value.
fn mandatory_resolve(resolve: u64) -> u64 {
    resolve | RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS
}

/// Open `name` inside an already-pinned directory. `resolve` is the mask the
/// directory was pinned with, so a child is never less restricted than its
/// parent: symlinks and magic links are always refused, and mount crossing
/// follows the grant (only an explicit `/` root may cross one).
fn open_child(
    dir: &OwnedFd,
    name: &std::ffi::OsStr,
    flags: i32,
    resolve: u64,
) -> io::Result<OwnedFd> {
    openat2(dir, Path::new(name), flags, 0, mandatory_resolve(resolve))
}

fn unlink_at(dir: &OwnedFd, name: &Path) {
    if let Ok(name) = CString::new(name.as_os_str().as_bytes()) {
        // SAFETY: `dir` is an open directory descriptor and `name` owns its
        // NUL-terminated buffer for the duration of the call.
        unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) };
    }
}

fn rename_at(dir: &OwnedFd, from: &Path, to: &Path) -> io::Result<()> {
    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in filesystem path"))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in filesystem path"))?;
    // SAFETY: both paths are NUL-terminated buffers that outlive the call, and
    // `dir` is an open directory descriptor.
    let result =
        unsafe { libc::renameat(dir.as_raw_fd(), from.as_ptr(), dir.as_raw_fd(), to.as_ptr()) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Install `from` as `to` only when `to` does not exist. `linkat` is the
/// create-only primitive: unlike rename, it cannot replace an existing name.
fn link_at(dir: &OwnedFd, from: &Path, to: &Path) -> io::Result<()> {
    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in filesystem path"))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in filesystem path"))?;
    // SAFETY: both paths are NUL-terminated buffers that outlive the call, and
    // `dir` is an open directory descriptor.
    let result = unsafe {
        libc::linkat(
            dir.as_raw_fd(),
            from.as_ptr(),
            dir.as_raw_fd(),
            to.as_ptr(),
            0,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Resolve the parent directory of `rel` beneath `root`, creating missing
/// components on the way. Returns the pinned parent and the file name.
fn open_parent_dir<'a>(
    root: &Root,
    rel: &'a Path,
    target: &Path,
    create_missing: bool,
) -> Result<(OwnedFd, &'a std::ffi::OsStr)> {
    let Some(file_name) = rel.file_name() else {
        bail!("cannot replace an authorized root directory");
    };
    let mut current: OwnedFd = openat2(
        &root.fd,
        Path::new("."),
        PATH_ONLY | libc::O_DIRECTORY,
        0,
        root.resolve,
    )?;
    let parent = rel.parent().unwrap_or_else(|| Path::new(""));
    for component in parent.components() {
        let Component::Normal(name) = component else {
            bail!("unexpected component in relative path {}", target.display());
        };
        let child = Path::new(name);
        match openat2(
            &current,
            child,
            PATH_ONLY | libc::O_DIRECTORY,
            0,
            root.resolve,
        ) {
            Ok(fd) => current = fd,
            Err(error) if error.kind() == io::ErrorKind::NotFound && !create_missing => {
                return Err(error)
                    .with_context(|| format!("missing parent directory in {}", target.display()));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                mkdirat(&current, child).with_context(|| {
                    format!(
                        "failed creating directory inside grant: {}",
                        target.display()
                    )
                })?;
                current = openat2(
                    &current,
                    child,
                    PATH_ONLY | libc::O_DIRECTORY,
                    0,
                    root.resolve,
                )?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("unsafe parent in {}", target.display()));
            }
        }
    }
    Ok((current, file_name))
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

/// What a staged write will do when committed.
#[derive(Clone, Copy, Debug)]
enum StageMode {
    Create,
    Replace,
    /// Existing-file update; the tuple is the expected length and SHA-256.
    Update((u64, [u8; 32])),
}

/// A replacement that is written and flushed but not yet visible. `commit`
/// either renames it over an existing target or links it into a free name;
/// dropping it removes the temporary.
#[derive(Debug)]
pub struct StagedWrite {
    parent: OwnedFd,
    name: std::ffi::OsString,
    target: PathBuf,
    temporary: std::ffi::OsString,
    resolve: u64,
    create_only: bool,
    /// Length and SHA-256 of the bytes the caller planned against. Updates
    /// re-check this immediately before the commit.
    expected: Option<(u64, [u8; 32])>,
    committed: bool,
}

impl StagedWrite {
    /// Make the staged bytes visible. Installing the name is the only step
    /// after the data is durable, so a failed write can never leave a
    /// half-written target.
    pub fn commit(mut self) -> Result<()> {
        if self.create_only {
            link_at(
                &self.parent,
                Path::new(&self.temporary),
                Path::new(&self.name),
            )?;
            // The target now owns the inode. The temporary name is redundant;
            // removing it is best effort because the requested file exists.
            unlink_at(&self.parent, Path::new(&self.temporary));
        } else {
            // Re-check immediately before the rename. A target that became a
            // symlink or special file, or whose bytes changed after the
            // snapshot was verified, is refused rather than silently replaced.
            if let Some((len, digest)) = self.expected {
                let fd = openat2(
                    &self.parent,
                    Path::new(&self.name),
                    libc::O_RDONLY | libc::O_NONBLOCK,
                    0,
                    self.resolve,
                )
                .with_context(|| format!("{} changed before commit", self.target.display()))?;
                let mut file = File::from(fd);
                let metadata = file.metadata()?;
                if !metadata.is_file() || metadata.len() != len {
                    bail!("{} changed before commit", self.target.display());
                }
                let mut hasher = Sha256::new();
                let mut buffer = [0_u8; 64 * 1024];
                let mut total = 0_u64;
                loop {
                    let read = file.read(&mut buffer)?;
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
            match openat2(
                &self.parent,
                Path::new(&self.name),
                PATH_ONLY | libc::O_NOFOLLOW,
                0,
                self.resolve,
            ) {
                Ok(fd) => {
                    if !File::from(fd).metadata()?.is_file() {
                        bail!(
                            "path is not a regular file: {}",
                            self.name.to_string_lossy()
                        );
                    }
                }
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound && self.expected.is_some() =>
                {
                    bail!("{} changed before commit", self.target.display())
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "refused unsafe write target {}",
                            self.name.to_string_lossy()
                        )
                    });
                }
            }
            rename_at(
                &self.parent,
                Path::new(&self.temporary),
                Path::new(&self.name),
            )?;
        }
        self.committed = true;
        // The commit contract is atomic visibility, not guaranteed crash
        // durability. Bytes were synced before rename; directory sync is best
        // effort because the containing directory may be writable but not
        // readable. Never report an error after a successful rename: a retry
        // could otherwise replay an already-committed edit.
        if let Ok(dir) = openat2(
            &self.parent,
            Path::new("."),
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
            self.resolve,
        ) {
            // SAFETY: dir is an owned descriptor that stays alive for fsync.
            if unsafe { libc::fsync(dir.as_raw_fd()) } != 0 {
                let error = io::Error::last_os_error();
                tracing::warn!(%error, path = %self.target.display(),
                    "file committed, but directory durability could not be confirmed");
            }
        }
        Ok(())
    }
}

impl Drop for StagedWrite {
    fn drop(&mut self) {
        if !self.committed {
            unlink_at(&self.parent, Path::new(&self.temporary));
        }
    }
}

#[derive(Debug)]
struct Root {
    path: PathBuf,
    fd: OwnedFd,
    resolve: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileIdentity {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) rdev: u64,
}

pub(crate) fn file_identity(file: &File) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file.metadata()?;
    Ok(FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        rdev: metadata.rdev(),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RootIdentity {
    pub(crate) path: PathBuf,
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) rdev: u64,
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
                    PATH_ONLY | libc::O_DIRECTORY,
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
                PATH_ONLY | libc::O_DIRECTORY,
                0,
                root.resolve,
            )
            .context("secure filesystem access requires Linux openat2 (5.6+)")?;
        }
        Ok(Self { read, write })
    }

    pub(crate) fn authority_roots(&self) -> Result<(Vec<RootIdentity>, Vec<RootIdentity>)> {
        let identities = |roots: &[Root]| -> Result<Vec<RootIdentity>> {
            let mut values = Vec::with_capacity(roots.len());
            for root in roots {
                let file = File::from(root.fd.try_clone()?);
                let identity = file_identity(&file)?;
                values.push(RootIdentity {
                    path: root.path.clone(),
                    dev: identity.dev,
                    ino: identity.ino,
                    rdev: identity.rdev,
                });
            }
            values.sort_by(|left, right| left.path.cmp(&right.path));
            Ok(values)
        };
        Ok((identities(&self.read)?, identities(&self.write)?))
    }

    fn root<'a, 'b>(&'a self, target: &'b Path, write: bool) -> Result<(&'a Root, &'b Path)> {
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

    /// Prepare a create-or-replace write. This needs only write authority, so
    /// it uses the write root rather than a possibly read-only narrower grant.
    pub fn stage(&self, path: &Path, bytes: &[u8]) -> Result<StagedWrite> {
        self.stage_in_root(self.root(path, true)?.0, path, bytes, StageMode::Replace)
    }

    /// Prepare an in-place update. This requires both grants and uses the
    /// narrower of them, so the bytes cannot land under a broader write-only
    /// grant than the one the snapshot was verified against. `expected` is the
    /// length and SHA-256 the caller planned against; commit re-checks it.
    pub fn stage_update(
        &self,
        path: &Path,
        bytes: &[u8],
        expected: (u64, [u8; 32]),
    ) -> Result<StagedWrite> {
        self.stage_in_root(
            self.replacement_root(path)?,
            path,
            bytes,
            StageMode::Update(expected),
        )
    }

    /// Prepare a create-only write. The target is not created until `commit`,
    /// which installs it with `linkat`, so a reader never sees an empty file
    /// and an existing target cannot be replaced.
    pub fn stage_create(&self, path: &Path, bytes: &[u8]) -> Result<StagedWrite> {
        self.stage_in_root(self.root(path, true)?.0, path, bytes, StageMode::Create)
    }

    /// Stream a verified update into its descriptor-rooted temporary. The
    /// caller controls byte production; this layer owns ACL/mode, cleanup,
    /// fsync and commit-time content preconditions for every update format.
    pub fn stage_update_with(
        &self,
        path: &Path,
        expected: (u64, [u8; 32]),
        write: impl FnOnce(&mut File) -> Result<()>,
    ) -> Result<StagedWrite> {
        self.stage_in_root_with(
            self.replacement_root(path)?,
            path,
            StageMode::Update(expected),
            write,
        )
    }

    fn stage_in_root(
        &self,
        root: &Root,
        path: &Path,
        bytes: &[u8],
        mode: StageMode,
    ) -> Result<StagedWrite> {
        use std::io::Write;
        self.stage_in_root_with(root, path, mode, |file| {
            file.write_all(bytes)?;
            Ok(())
        })
    }

    fn stage_in_root_with(
        &self,
        root: &Root,
        path: &Path,
        mode: StageMode,
        write: impl FnOnce(&mut File) -> Result<()>,
    ) -> Result<StagedWrite> {
        let rel = path
            .strip_prefix(&root.path)
            .map_err(|_| anyhow!("path has no authorized root: {}", path.display()))?;
        let (parent, file_name) = open_parent_dir(root, rel, path, true)?;
        self.stage_at(&parent, file_name, root.resolve, path, mode, write)
    }

    /// The replacement root is the deeper of the read and write grants: an
    /// edit/update requires both capabilities, and the narrower pinned inode is
    /// the one the snapshot was verified against.
    fn replacement_root(&self, path: &Path) -> Result<&Root> {
        let (read_root, _) = self.root(path, false)?;
        let (write_root, _) = self.root(path, true)?;
        let read_depth = read_root.path.components().count();
        match read_depth >= write_root.path.components().count() {
            true => Ok(read_root),
            false => Ok(write_root),
        }
    }

    /// Stage `bytes` for `name` inside an already-pinned parent directory.
    fn stage_at(
        &self,
        parent: &OwnedFd,
        name: &std::ffi::OsStr,
        resolve: u64,
        target: &Path,
        mode: StageMode,
        write: impl FnOnce(&mut File) -> Result<()>,
    ) -> Result<StagedWrite> {
        let resolve = mandatory_resolve(resolve);
        let create_only = matches!(mode, StageMode::Create);
        // Keep the mode of an existing regular file (`+x` scripts, 0600 keys).
        // A symlink or non-file target is refused, never followed and never
        // replaced, so the rename below can only land on a regular file.
        let existing_mode = match openat2(
            parent,
            Path::new(name),
            PATH_ONLY | libc::O_NOFOLLOW,
            0,
            resolve,
        ) {
            Ok(fd) if create_only => {
                drop(fd);
                bail!("path already exists: {}", target.display());
            }
            Ok(fd) => {
                let metadata = File::from(fd).metadata()?;
                if !metadata.is_file() {
                    bail!("path is not a regular file: {}", target.display());
                }
                Some(metadata.permissions().mode() & 0o7777)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("refused unsafe write target {}", target.display()));
            }
        };

        // Clone the pinned parent before creating the temporary, so a
        // descriptor-exhaustion failure cannot strand a temp with no owner.
        let staged_parent = parent.try_clone()?;

        let temporary = crate::staging::staging_name();
        // Create with the target's own mode (never a broader default), so a
        // private file is not world-readable while its replacement is written.
        // A target that does not exist yet keeps ordinary create semantics.
        let fd = openat2(
            parent,
            Path::new(&temporary),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            crate::staging::staging_mode(existing_mode),
            resolve,
        )
        .with_context(|| format!("refused a temporary file next to {}", target.display()))?;
        let mut file = File::from(fd);

        let written = (|| -> Result<()> {
            write(&mut file)?;
            if let Some(mode) = existing_mode {
                let mode = libc::mode_t::try_from(mode)
                    .expect("masked Unix permission bits must fit mode_t");
                // SAFETY: `file` is an open descriptor owned by this scope.
                if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
                    return Err(io::Error::last_os_error()).context("failed preserving file mode");
                }
            }
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = written {
            drop(file);
            unlink_at(parent, Path::new(&temporary));
            return Err(error);
        }
        drop(file);
        let expected = match mode {
            StageMode::Update(expected) => Some(expected),
            StageMode::Create | StageMode::Replace => None,
        };
        Ok(StagedWrite {
            parent: staged_parent,
            name: name.to_owned(),
            target: target.to_path_buf(),
            temporary,
            resolve,
            create_only,
            expected,
            committed: false,
        })
    }

    /// Open a child of an already-pinned directory as a regular file. No root
    /// lookup and no path resolution happen here: `name` is one component of a
    /// directory the policy already authorized.
    pub fn open_child_read(
        &self,
        dir: &OwnedFd,
        name: &std::ffi::OsStr,
        resolve: u64,
    ) -> Result<File> {
        let file = File::from(open_child(
            dir,
            name,
            libc::O_RDONLY | libc::O_NONBLOCK,
            resolve,
        )?);
        if !file.metadata()?.is_file() {
            bail!("path is not a regular file: {}", name.to_string_lossy());
        }
        Ok(file)
    }

    /// Open a child directory of an already-pinned directory, for descent.
    pub fn open_child_dir(
        &self,
        dir: &OwnedFd,
        name: &std::ffi::OsStr,
        resolve: u64,
    ) -> Result<File> {
        Ok(File::from(open_child(
            dir,
            name,
            libc::O_RDONLY | libc::O_DIRECTORY,
            resolve,
        )?))
    }

    /// The resolve mask the read root that owns `path` was pinned with, so the
    /// walk can open children with the same restrictions.
    pub fn read_resolve(&self, path: &Path) -> Result<u64> {
        self.root(path, false).map(|(root, _)| root.resolve)
    }

    pub fn open_directory(&self, path: &Path) -> Result<File> {
        self.open(path, false, libc::O_RDONLY | libc::O_DIRECTORY, 0)
    }

    /// Open an opted-in host service/config path by FD, refusing symlink
    /// replacement and verifying the actual pinned object type. Only the
    /// explicitly requested object is mounted; no parent directory authority
    /// is shared with the sandbox.
    #[cfg(target_os = "linux")]
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
            PATH_ONLY | libc::O_NOFOLLOW,
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

    /// Pin a path just resolved as canonical, for a sandbox bind. A symlink
    /// that appeared since fails the open instead of redirecting the bind.
    #[cfg(target_os = "linux")]
    pub fn pin_canonical(path: &Path) -> Result<File> {
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
            PATH_ONLY | libc::O_NOFOLLOW,
            0,
            RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
        )
        .with_context(|| format!("unsafe sandbox mount source {}", path.display()))?;
        Ok(File::from(fd))
    }

    /// Pin an explicitly configured shell-only cache source to the directory
    /// validated at startup. This is not an MCP filesystem grant.
    #[cfg(target_os = "linux")]
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

    /// A temporary tree whose base is already in its resolved spelling. The policy
    /// resolves every authorized root before it reaches this layer, and macOS
    /// reaches `$TMPDIR` through the `/var` symlink, so a raw `tempdir()` path is
    /// refused by the pinned-root walk on purpose.
    fn resolved_tempdir() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        (temp, base)
    }

    #[test]
    fn portable_openat_walk_rejects_symlink_swaps() {
        let (_temp, root) = resolved_tempdir();
        let allowed = root.join("allowed");
        let other = root.join("other");
        fs::create_dir_all(&allowed).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("secret"), b"do not touch").unwrap();
        std::os::unix::fs::symlink(&other, allowed.join("jump")).unwrap();
        let dir: OwnedFd = File::open(&allowed).unwrap().into();
        assert!(
            openat_beneath(
                &dir,
                Path::new("jump/secret"),
                libc::O_RDONLY,
                0,
                SAFE_RESOLVE
            )
            .is_err()
        );
        assert!(
            openat_beneath(
                &dir,
                Path::new("../other/secret"),
                libc::O_RDONLY,
                0,
                SAFE_RESOLVE
            )
            .is_err()
        );
        assert!(
            openat_beneath(
                &dir,
                Path::new("future"),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
                SAFE_RESOLVE
            )
            .is_ok()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn child_metadata_obeys_mount_boundary_and_explicit_root_exception() {
        use std::os::unix::fs::MetadataExt as _;
        let directory: OwnedFd = File::open("/").unwrap().into();
        let root_device = fs::metadata("/").unwrap().dev();
        // Common Linux mountpoints; skip only on unusual test hosts without a
        // distinct mount. No privileged mount namespace is required.
        let mount = ["proc", "sys", "dev"].into_iter().find(|name| {
            fs::metadata(format!("/{name}")).is_ok_and(|meta| meta.dev() != root_device)
        });
        let Some(mount) = mount else { return };
        let denied = child_metadata(&directory, mount.as_ref(), SAFE_RESOLVE).unwrap_err();
        assert_eq!(denied.raw_os_error(), Some(libc::EXDEV));
        let allowed = child_metadata(
            &directory,
            mount.as_ref(),
            RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
        )
        .unwrap();
        assert_eq!(allowed.kind, "dir");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn child_metadata_lists_symlinks_without_following_them() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::write(root.join("file"), b"a").unwrap();
        symlink("file", root.join("alias")).unwrap();
        let directory: OwnedFd = File::open(&root).unwrap().into();
        let file = child_metadata(&directory, "file".as_ref(), SAFE_RESOLVE).unwrap();
        let alias = child_metadata(&directory, "alias".as_ref(), SAFE_RESOLVE).unwrap();
        assert_eq!(file.kind, "file");
        assert_eq!(file.len, 1);
        assert_eq!(alias.kind, "symlink");
        assert_eq!(alias.len, 4);
    }

    #[test]
    fn streamed_update_preserves_mode_and_cleans_failed_temporary() {
        use std::io::Write as _;
        let (_temp, root) = resolved_tempdir();
        let path = root.join("private.txt");
        std::fs::write(&path, b"before").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let sandbox =
            SecureFs::new(std::slice::from_ref(&root), std::slice::from_ref(&root)).unwrap();
        let expected = (6, crate::mcp::sha256(b"before"));
        let error = sandbox
            .stage_update_with(&path, expected, |file| {
                file.write_all(b"partial")?;
                anyhow::bail!("synthetic streaming failure")
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("synthetic streaming failure"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), b"before");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);

        let staged = sandbox
            .stage_update_with(&path, expected, |file| {
                file.write_all(b"after")?;
                Ok(())
            })
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"before");
        staged.commit().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"after");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn securely_denies_redirected_symlinks_and_writes_inside_grant() {
        let (_temp, base) = resolved_tempdir();
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), "secret").unwrap();
        let sandbox =
            SecureFs::new(std::slice::from_ref(&root), std::slice::from_ref(&root)).unwrap();

        // The original path may have been approved, but it must not be
        // reopened through an alias that changed to an external directory.
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        assert!(sandbox.open_read(&root.join("escape/secret")).is_err());
        assert!(sandbox.stage(&root.join("escape/new"), b"x").is_err());
        assert!(!outside.join("new").exists());

        let file = sandbox
            .stage_create(&root.join("nested/new.txt"), b"hello")
            .unwrap();
        file.commit().unwrap();
        assert_eq!(fs::read(root.join("nested/new.txt")).unwrap(), b"hello");
    }

    #[test]
    fn concurrent_symlink_swaps_cannot_redirect_opened_handles() {
        use std::io::Read;
        use std::os::unix::fs::symlink;

        let (_temp, base) = resolved_tempdir();
        let root = base.join("granted");
        let outside = base.join("outside");
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
                if let Ok(staged) = safe.stage(&selected, b"local") {
                    let _ = staged.commit();
                }
            }
        });
        assert_eq!(fs::read_to_string(secret).unwrap(), "never-touch");
    }

    #[test]
    fn edit_requires_both_pinned_read_and_write_grants() {
        let (_temp, base) = resolved_tempdir();
        let root = base.join("root");
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
        let (_temp, base) = resolved_tempdir();
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"secret").unwrap();
        let sandbox =
            SecureFs::new(std::slice::from_ref(&root), std::slice::from_ref(&root)).unwrap();

        fs::write(root.join("present"), b"present").unwrap();
        assert!(sandbox.stage_create(&root.join("present"), b"new").is_err());
        assert_eq!(fs::read(root.join("present")).unwrap(), b"present");

        std::os::unix::fs::symlink(outside.join("secret"), root.join("alias")).unwrap();
        assert!(sandbox.stage_create(&root.join("alias"), b"new").is_err());
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"secret");

        // The target is absent until commit, then appears with the whole payload.
        let staged = sandbox
            .stage_create(&root.join("nested/new.txt"), b"hello")
            .unwrap();
        assert!(!root.join("nested/new.txt").exists());
        staged.commit().unwrap();
        assert_eq!(fs::read(root.join("nested/new.txt")).unwrap(), b"hello");
    }

    #[test]
    fn atomic_write_replaces_content_keeps_mode_and_refuses_symlinks() {
        let (_temp, base) = resolved_tempdir();
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"secret").unwrap();
        let sandbox =
            SecureFs::new(std::slice::from_ref(&root), std::slice::from_ref(&root)).unwrap();

        // A new file appears with the whole payload and no leftover temp file.
        sandbox
            .stage(&root.join("nested/new.txt"), b"first")
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(fs::read(root.join("nested/new.txt")).unwrap(), b"first");
        assert_eq!(fs::read_dir(root.join("nested")).unwrap().count(), 1);

        // Replacing keeps the permission bits of the existing file.
        let script = root.join("script.sh");
        fs::write(&script, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o750)).unwrap();
        sandbox
            .stage(&script, b"#!/bin/sh\necho hi\n")
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(
            fs::read(&script).unwrap(),
            b"#!/bin/sh\necho hi\n",
            "content replaced"
        );
        assert_eq!(
            fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o750
        );

        // A symlink target is refused and its target is untouched.
        std::os::unix::fs::symlink(outside.join("secret"), root.join("alias")).unwrap();
        assert!(sandbox.stage(&root.join("alias"), b"evil").is_err());
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"secret");
        assert!(root.join("alias").is_symlink());

        // A directory target is refused rather than renamed over.
        fs::create_dir(root.join("dir")).unwrap();
        assert!(sandbox.stage(&root.join("dir"), b"x").is_err());

        // A name at the filesystem's length limit still gets a replaceable
        // temporary, and no temporary is left behind.
        let long = "n".repeat(200);
        sandbox
            .stage(&root.join(&long), b"first")
            .unwrap()
            .commit()
            .unwrap();
        sandbox
            .stage(&root.join(&long), b"second")
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(fs::read(root.join(&long)).unwrap(), b"second");
        assert!(
            fs::read_dir(&root)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .all(|entry| !entry.file_name().to_string_lossy().contains(".dotlink-"))
        );
    }

    /// A replacement of a private file must never be readable by another user
    /// while it is being written. The staging temporary starts at the target's
    /// mode, not at 0666-minus-umask narrowed only after the payload is written.
    #[test]
    fn staging_never_widens_a_private_target_mid_write() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        };

        let (_temp, base) = resolved_tempdir();
        let root = base.join("root");
        fs::create_dir(&root).unwrap();
        // A traversable parent is what makes the temporary observable at all.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let secret = root.join("secret");
        fs::write(&secret, b"old").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        let sandbox =
            SecureFs::new(std::slice::from_ref(&root), std::slice::from_ref(&root)).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::<(u32, u64)>::new()));
        let scanner = {
            let root = root.clone();
            let stop = stop.clone();
            let seen = seen.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    for entry in fs::read_dir(&root).into_iter().flatten().flatten() {
                        if !crate::staging::is_staging_temporary(&entry.file_name()) {
                            continue;
                        }
                        let Ok(metadata) = entry.metadata() else {
                            continue;
                        };
                        let mode = metadata.permissions().mode() & 0o777;
                        let mut seen = seen.lock().unwrap();
                        if !seen.contains(&(mode, metadata.len())) {
                            seen.push((mode, metadata.len()));
                        }
                    }
                }
            })
        };

        let payload = vec![b'x'; 8 * 1024 * 1024];
        let staged = sandbox.stage(&secret, &payload).unwrap();
        stop.store(true, Ordering::Relaxed);
        scanner.join().unwrap();
        staged.commit().unwrap();
        assert_eq!(
            fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty(), "the temporary was never observed");
        for (mode, _) in seen.iter() {
            assert_eq!(
                mode & !0o600,
                0,
                "staging a 0600 file produced a broader temporary: {seen:?}"
            );
        }
    }

    #[test]
    fn staged_update_refuses_bytes_changed_after_preflight() {
        let (_temp, base) = resolved_tempdir();
        let root = base.join("root");
        fs::create_dir(&root).unwrap();
        let path = root.join("target");
        fs::write(&path, b"old").unwrap();
        let sandbox =
            SecureFs::new(std::slice::from_ref(&root), std::slice::from_ref(&root)).unwrap();

        let expected = (3, crate::mcp::sha256(b"old"));
        let staged = sandbox.stage_update(&path, b"new", expected).unwrap();
        fs::write(&path, b"external").unwrap();
        let error = staged.commit().unwrap_err().to_string();
        assert!(error.contains("changed before commit"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"external");
        assert!(
            fs::read_dir(&root)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .all(|entry| !entry.file_name().to_string_lossy().contains(".dotlink-"))
        );
    }

    #[test]
    fn staged_replacement_rechecks_a_target_swapped_for_a_symlink() {
        let (_temp, base) = resolved_tempdir();
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(root.join("target"), b"old").unwrap();
        fs::write(outside.join("secret"), b"secret").unwrap();
        let sandbox =
            SecureFs::new(std::slice::from_ref(&root), std::slice::from_ref(&root)).unwrap();

        let staged = sandbox.stage(&root.join("target"), b"new").unwrap();
        fs::remove_file(root.join("target")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), root.join("target")).unwrap();
        let error = staged.commit().unwrap_err().to_string();
        assert!(
            error.contains("regular file") || error.contains("unsafe"),
            "{error}"
        );
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"secret");
        assert!(root.join("target").is_symlink());
        assert!(
            fs::read_dir(&root)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .all(|entry| !entry.file_name().to_string_lossy().contains(".dotlink-"))
        );
    }

    #[test]
    fn child_opens_stay_inside_the_pinned_directory() {
        use std::io::Read;

        let (_temp, base) = resolved_tempdir();
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(root.join("a.txt"), b"a").unwrap();
        fs::write(outside.join("secret"), b"secret").unwrap();
        let sandbox =
            SecureFs::new(std::slice::from_ref(&root), std::slice::from_ref(&root)).unwrap();

        let dir: OwnedFd = sandbox.open_directory(&root).unwrap().into();
        let mut opened = sandbox
            .open_child_read(
                &dir,
                "a.txt".as_ref(),
                RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
            )
            .unwrap();
        let mut text = String::new();
        opened.read_to_string(&mut text).unwrap();
        assert_eq!(text, "a");

        assert!(
            sandbox
                .open_child_read(
                    &dir,
                    "missing".as_ref(),
                    RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS
                )
                .is_err()
        );
        // A directory opens for descent but never as a file.
        fs::create_dir(root.join("sub")).unwrap();
        assert!(
            sandbox
                .open_child_dir(&dir, "sub".as_ref(), RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)
                .is_ok()
        );
        assert!(
            sandbox
                .open_child_read(&dir, "sub".as_ref(), RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)
                .is_err()
        );
        // One component only: neither an empty name nor traversal escapes.
        assert!(
            sandbox
                .open_child_read(&dir, "".as_ref(), RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)
                .is_err()
        );
        assert!(
            sandbox
                .open_child_read(
                    &dir,
                    outside.join("secret").as_os_str(),
                    RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
                )
                .is_err()
        );
        assert!(
            sandbox
                .open_child_read(
                    &dir,
                    "../outside/secret".as_ref(),
                    RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
                )
                .is_err()
        );
    }

    #[test]
    fn root_capability_cannot_operate_outside_itself() {
        let (_temp, base) = resolved_tempdir();
        let one = base.join("one");
        let two = base.join("two");
        fs::create_dir(&one).unwrap();
        fs::create_dir(&two).unwrap();
        let secure = SecureFs::new(std::slice::from_ref(&one), std::slice::from_ref(&one)).unwrap();
        assert!(secure.stage(&two.join("file"), b"x").is_err());
        assert!(!two.join("file").exists());
    }
}
