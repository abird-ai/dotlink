//! Mutation coordination across MCP calls and independent dotlink processes.
//!
//! The target inode changes on atomic replacement. Lock its containing
//! directory instead: a stable OS object with no workspace lockfiles, no XDG
//! dependency, and no assumption about another process's mount namespace.

use std::{
    collections::{BTreeSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::Path,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use tokio::sync::{Mutex, OwnedMutexGuard};

#[cfg(unix)]
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Weak,
};

const LOCK_SHARDS: usize = 256;
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(unix)]
const LOCK_RETRY: Duration = Duration::from_millis(2);
#[cfg(unix)]
const REGISTRY_PRUNE_AT: usize = 512;
static SHARDS: OnceLock<Box<[Arc<Mutex<()>>]>> = OnceLock::new();

fn shards() -> &'static [Arc<Mutex<()>>] {
    SHARDS.get_or_init(|| (0..LOCK_SHARDS).map(|_| Arc::new(Mutex::new(()))).collect())
}

fn shard(path: &Path) -> usize {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    (hasher.finish() as usize) % LOCK_SHARDS
}

/// Held from snapshot read through the last commit, with deterministic
/// acquisition and automatic release on cancellation, error, or process exit.
pub(crate) struct MutationGuard {
    _shards: Vec<OwnedMutexGuard<()>>,
    #[cfg(unix)]
    _directories: Vec<Arc<DirectoryLock>>,
    #[cfg(windows)]
    _windows: Vec<WindowsMutationLock>,
}

#[cfg(unix)]
type DirectoryRegistry = std::sync::Mutex<HashMap<(u64, u64, LockMode), Weak<DirectoryLock>>>;
#[cfg(unix)]
static DIRECTORIES: OnceLock<DirectoryRegistry> = OnceLock::new();

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum LockMode {
    Intent,
    Exclusive,
}

#[cfg(unix)]
struct DirectoryLock {
    _file: std::fs::File,
}

/// Lock an existing directory precisely. Never fall back to another inode:
/// that would let two writers choose different locks for one target.
#[cfg(unix)]
impl DirectoryLock {
    async fn acquire(path: &Path, mode: LockMode, deadline: Instant) -> Result<Arc<Self>> {
        use std::os::{fd::AsRawFd as _, unix::fs::MetadataExt as _};

        let file = std::fs::File::open(path).map_err(|error| {
            anyhow::anyhow!("cannot lock mutation directory {}: {error}", path.display())
        })?;
        let metadata = file.metadata()?;
        if !metadata.is_dir() {
            bail!("mutation directory is not a directory: {}", path.display());
        }
        let identity = (metadata.dev(), metadata.ino(), mode);
        let registry = DIRECTORIES.get_or_init(Default::default);
        let operation = match mode {
            LockMode::Intent => libc::LOCK_SH,
            LockMode::Exclusive => libc::LOCK_EX,
        };
        loop {
            if let Some(existing) = registry
                .lock()
                .expect("directory lock registry")
                .get(&identity)
                .and_then(Weak::upgrade)
            {
                return Ok(existing);
            }
            // SAFETY: `file` is open and owned until the lock's final guard
            // drops. Nonblocking retries never block a Tokio executor thread.
            if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
                let locked = Arc::new(Self { _file: file });
                let mut entries = registry.lock().expect("directory lock registry");
                if let Some(existing) = entries.get(&identity).and_then(Weak::upgrade) {
                    return Ok(existing);
                }
                if entries.len() >= REGISTRY_PRUNE_AT {
                    entries.retain(|_, weak| weak.strong_count() > 0);
                }
                entries.insert(identity, Arc::downgrade(&locked));
                return Ok(locked);
            }
            let error = std::io::Error::last_os_error();
            match error.kind() {
                std::io::ErrorKind::WouldBlock if Instant::now() < deadline => {
                    tokio::time::sleep(LOCK_RETRY).await;
                }
                std::io::ErrorKind::WouldBlock => bail!(
                    "another process is mutating {}; retry when it finishes",
                    path.display()
                ),
                std::io::ErrorKind::Interrupted => {}
                _ => return Err(error.into()),
            }
        }
    }
}

/// Existing ancestors take intention (SH) locks, and the closest existing
/// parent takes an exclusive (EX) lock. If a parent is not yet present,
/// creating it is covered by its nearest existing ancestor's EX lock.
/// A second process that sees the newly created directory must acquire an SH
/// lock on that ancestor first, so it cannot bypass the first writer.
/// Locking an unreadable ancestor fails closed rather than substituting a
/// different directory. This preserves automatic parent creation safely.
#[cfg(unix)]
fn lock_plan(paths: &[&Path]) -> Result<BTreeMap<PathBuf, LockMode>> {
    let mut plan = BTreeMap::new();
    for target in paths {
        let parent = target.parent().ok_or_else(|| {
            anyhow::anyhow!("cannot lock root for file mutation: {}", target.display())
        })?;
        if !parent.is_absolute() {
            bail!("mutation paths must be absolute: {}", target.display());
        }
        let mut current = PathBuf::new();
        let mut nearest = None;
        for component in parent.components() {
            current.push(component.as_os_str());
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.is_dir() => {
                    // Missing execute/read permission cannot be bypassed by
                    // falling back to a readable ancestor.
                    nearest = Some(current.clone());
                    plan.entry(current.clone()).or_insert(LockMode::Intent);
                }
                Ok(_) => bail!("unsafe mutation parent directory: {}", current.display()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => bail!(
                    "cannot inspect mutation parent {}: {error}",
                    current.display()
                ),
            }
        }
        let nearest = nearest
            .ok_or_else(|| anyhow::anyhow!("no existing directory for {}", target.display()))?;
        plan.insert(nearest, LockMode::Exclusive);
    }
    Ok(plan)
}

/// Windows kernel mutexes work across processes and logon sessions, are
/// abandoned automatically on process/thread death and leave no lockfiles.
/// Mutex ownership is thread-affine, so the acquiring thread waits for a drop
/// notification and releases it itself. A cancelled async request drops that
/// notification; it can never strand the kernel mutex.
#[cfg(windows)]
struct WindowsMutationLock {
    done: Option<std::sync::mpsc::Sender<()>>,
}

#[cfg(windows)]
impl Drop for WindowsMutationLock {
    fn drop(&mut self) {
        drop(self.done.take());
    }
}

#[cfg(windows)]
fn windows_mutex_name(path: &Path) -> String {
    use sha2::{Digest as _, Sha256};
    // NTFS is case-insensitive by default, unlike the Linux path namespace.
    // Hash the entire canonical path: an atomic rename changes the inode, not
    // the path-based lock identity.
    let canonical = path.to_string_lossy().to_lowercase();
    let digest = Sha256::digest(canonical.as_bytes());
    format!("Global\\abird-dotlink-path-{}", hex::encode(digest))
}

#[cfg(windows)]
impl WindowsMutationLock {
    async fn acquire(path: &Path, deadline: Instant) -> Result<Self> {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
        };
        let identity = windows_mutex_name(path);
        let display = path.display().to_string();
        let timeout = deadline.saturating_duration_since(Instant::now());
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let (ready, wait) = tokio::sync::oneshot::channel::<Result<std::sync::mpsc::Sender<()>>>();
        std::thread::Builder::new()
            .name("dotlink-file-lock".into())
            .spawn(move || {
                let wide = identity.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
                // SAFETY: NUL-terminated stable name, kernel object scope and
                // ownership are validated by the OS on creation/open.
                let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
                if handle.is_null() {
                    let _ = ready.send(Err(std::io::Error::last_os_error().into()));
                    return;
                }
                // SAFETY: handle is a live Windows mutex. Blocking occurs
                // on this dedicated OS thread, never on a Tokio executor.
                let code = unsafe { WaitForSingleObject(handle, millis) };
                if code == WAIT_OBJECT_0 || code == WAIT_ABANDONED {
                    let (released, closed) = std::sync::mpsc::channel::<()>();
                    if ready.send(Ok(released)).is_ok() {
                        // A dropped guard or receiver releases from the same
                        // OS thread that acquired the mutex.
                        let _ = closed.recv();
                    }
                    // SAFETY: the mutex belongs to the current OS thread.
                    if unsafe { ReleaseMutex(handle) } == 0 {
                        tracing::warn!(path = %display, "failed releasing Windows mutation mutex");
                    }
                } else {
                    let reason = if code == WAIT_TIMEOUT {
                        anyhow::anyhow!("another process is mutating {display}; retry")
                    } else {
                        std::io::Error::last_os_error().into()
                    };
                    let _ = ready.send(Err(reason));
                }
                // SAFETY: CreateMutexW returned an owned kernel object handle.
                unsafe { CloseHandle(handle) };
            })
            .map_err(|error| anyhow::anyhow!("cannot start Windows file-lock worker: {error}"))?;
        Ok(Self {
            done: Some(wait.await??),
        })
    }
}

pub(crate) async fn lock<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Result<MutationGuard> {
    let paths = paths.into_iter().collect::<Vec<_>>();
    let indices = paths
        .iter()
        .map(|path| shard(path))
        .collect::<BTreeSet<_>>();
    let mut guards = Vec::with_capacity(indices.len());
    for index in indices {
        guards.push(shards()[index].clone().lock_owned().await);
    }

    #[cfg(unix)]
    let mut directories = Vec::new();
    #[cfg(unix)]
    {
        let deadline = Instant::now()
            .checked_add(LOCK_TIMEOUT)
            .expect("30-second lock deadline fits monotonic clock");
        for (path, mode) in lock_plan(&paths)? {
            directories.push(DirectoryLock::acquire(&path, mode, deadline).await?);
        }
    }
    #[cfg(windows)]
    let mut windows = Vec::new();
    #[cfg(windows)]
    {
        let deadline = Instant::now() + LOCK_TIMEOUT;
        let targets = paths
            .iter()
            .map(|path| path.to_path_buf())
            .collect::<BTreeSet<_>>();
        for path in targets {
            windows.push(WindowsMutationLock::acquire(&path, deadline).await?);
        }
    }
    Ok(MutationGuard {
        _shards: guards,
        #[cfg(unix)]
        _directories: directories,
        #[cfg(windows)]
        _windows: windows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_lock_keys_are_case_insensitive_and_stable() {
        assert_eq!(
            windows_mutex_name(Path::new("C:\\Repo\\DATA.TXT")),
            windows_mutex_name(Path::new("c:\\repo\\data.txt"))
        );
        assert_ne!(
            windows_mutex_name(Path::new("C:\\Repo\\a.txt")),
            windows_mutex_name(Path::new("C:\\Repo\\b.txt"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn parent_creation_is_locked_at_the_nearest_existing_ancestor() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().canonicalize().unwrap();
        let target = parent.join("new/child/data.txt");
        let plan = lock_plan(&[target.as_path()]).unwrap();
        assert_eq!(plan.get(&parent), Some(&LockMode::Exclusive));
        assert!(!plan.contains_key(&parent.join("new")));
        std::fs::create_dir_all(parent.join("new/child")).unwrap();
        let plan = lock_plan(&[target.as_path()]).unwrap();
        assert_eq!(plan.get(&parent), Some(&LockMode::Intent));
        assert_eq!(
            plan.get(&parent.join("new/child")),
            Some(&LockMode::Exclusive)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_only_parent_fails_closed() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("execute-only");
        std::fs::create_dir(&parent).unwrap();
        let original = std::fs::metadata(&parent).unwrap().permissions();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o300)).unwrap();
        let result = lock([parent.join("data.txt").as_path()]).await;
        std::fs::set_permissions(&parent, original).unwrap();
        assert!(
            result.is_err(),
            "an unreadable parent must not fall back to an ancestor"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn one_process_reuses_the_held_directory_descriptor() {
        let root = tempfile::tempdir().unwrap();
        let deadline = Instant::now() + LOCK_TIMEOUT;
        let a = DirectoryLock::acquire(root.path(), LockMode::Exclusive, deadline)
            .await
            .unwrap();
        let b = DirectoryLock::acquire(root.path(), LockMode::Exclusive, deadline)
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }
}
