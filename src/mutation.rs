//! Mutation coordination across MCP calls and independent dotlink processes.
//!
//! Inside one process, exact path mutexes plus stable Unix directory-entry
//! identities prevent conflicting aliases without serializing unrelated files.
//! Across Unix processes, target inodes change on atomic replacement, so lock
//! their stable parent directories instead: no workspace lockfiles or XDG
//! dependency.

use std::{
    collections::{BTreeSet, HashMap},
    hash::Hash,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex, OnceLock, Weak},
    time::{Duration, Instant},
};

use anyhow::Result;
#[cfg(unix)]
use anyhow::bail;
use tokio::sync::{Mutex, OwnedMutexGuard};

#[cfg(unix)]
use std::collections::BTreeMap;

const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(unix)]
const LOCK_RETRY: Duration = Duration::from_millis(2);
const REGISTRY_PRUNE_AT: usize = 512;

/// Weak keyed mutexes avoid unrelated hash-shard collisions while allowing
/// idle entries to disappear. The same registry primitive backs lexical path
/// coordination and Unix stable directory-entry identities.
struct LocalLockRegistry<K> {
    entries: StdMutex<HashMap<K, Weak<Mutex<()>>>>,
}

impl<K> Default for LocalLockRegistry<K> {
    fn default() -> Self {
        Self {
            entries: StdMutex::new(HashMap::new()),
        }
    }
}

impl<K> LocalLockRegistry<K>
where
    K: Clone + Eq + Hash,
{
    fn get(&self, key: &K) -> Arc<Mutex<()>> {
        let mut entries = self.entries.lock().expect("mutation local-lock registry");
        if let Some(lock) = entries.get(key).and_then(Weak::upgrade) {
            return lock;
        }
        if entries.len() >= REGISTRY_PRUNE_AT {
            entries.retain(|_, weak| weak.strong_count() > 0);
        }
        let lock = Arc::new(Mutex::new(()));
        entries.insert(key.clone(), Arc::downgrade(&lock));
        lock
    }
}

static PATH_LOCKS: OnceLock<LocalLockRegistry<PathBuf>> = OnceLock::new();

fn path_lock(path: &Path) -> Arc<Mutex<()>> {
    PATH_LOCKS
        .get_or_init(Default::default)
        .get(&path.to_path_buf())
}

/// Held from snapshot read through the last commit, with deterministic
/// acquisition and automatic release on cancellation, error, or process exit.
pub(crate) struct MutationGuard {
    _paths: Vec<OwnedMutexGuard<()>>,
    #[cfg(unix)]
    _targets: Vec<OwnedMutexGuard<()>>,
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
type DirectoryIdentity = (u64, u64);

#[cfg(unix)]
#[derive(Clone, Debug)]
struct PlannedDirectory {
    path: PathBuf,
    mode: LockMode,
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct UnixTargetKey {
    ancestor_dev: u64,
    ancestor_ino: u64,
    relative: PathBuf,
}

#[cfg(unix)]
static UNIX_TARGET_LOCKS: OnceLock<LocalLockRegistry<UnixTargetKey>> = OnceLock::new();

#[cfg(unix)]
fn unix_target_key(target: &Path) -> Result<UnixTargetKey> {
    use std::os::unix::fs::MetadataExt as _;

    let parent = target.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "cannot identify root for file mutation: {}",
            target.display()
        )
    })?;
    if !parent.is_absolute() {
        bail!("mutation paths must be absolute: {}", target.display());
    }
    let mut current = PathBuf::new();
    let mut nearest = None;
    for component in parent.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() => nearest = Some((current.clone(), metadata)),
            Ok(_) => bail!("unsafe mutation parent directory: {}", current.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => bail!(
                "cannot inspect mutation parent {}: {error}",
                current.display()
            ),
        }
    }
    let (ancestor, metadata) =
        nearest.ok_or_else(|| anyhow::anyhow!("no existing directory for {}", target.display()))?;
    let relative = target.strip_prefix(&ancestor).map_err(|_| {
        anyhow::anyhow!(
            "mutation target {} is not below its existing ancestor {}",
            target.display(),
            ancestor.display()
        )
    })?;
    if relative.as_os_str().is_empty() {
        bail!(
            "mutation target must name a file below {}",
            ancestor.display()
        );
    }
    Ok(UnixTargetKey {
        ancestor_dev: metadata.dev(),
        ancestor_ino: metadata.ino(),
        relative: relative.to_path_buf(),
    })
}

#[cfg(unix)]
fn unix_target_plan(paths: &[&Path]) -> Result<std::collections::BTreeMap<UnixTargetKey, PathBuf>> {
    let mut targets = std::collections::BTreeMap::new();
    for path in paths {
        targets
            .entry(unix_target_key(path)?)
            .or_insert_with(|| path.to_path_buf());
    }
    Ok(targets)
}

#[cfg(unix)]
struct DirectoryLock {
    _file: std::fs::File,
}

/// Lock an existing directory precisely. Never fall back to another inode:
/// that would let two writers choose different locks for one target.
#[cfg(unix)]
impl DirectoryLock {
    async fn acquire(
        path: &Path,
        expected: DirectoryIdentity,
        mode: LockMode,
        deadline: Instant,
    ) -> Result<Arc<Self>> {
        use std::os::{fd::AsRawFd as _, unix::fs::MetadataExt as _};

        let file = std::fs::File::open(path).map_err(|error| {
            anyhow::anyhow!("cannot lock mutation directory {}: {error}", path.display())
        })?;
        let metadata = file.metadata()?;
        if !metadata.is_dir() {
            bail!("mutation directory is not a directory: {}", path.display());
        }
        let found = (metadata.dev(), metadata.ino());
        if found != expected {
            bail!(
                "mutation directory changed while locking {} (expected {:?}, found {:?})",
                path.display(),
                expected,
                found,
            );
        }
        let identity = (expected.0, expected.1, mode);
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
fn lock_plan(paths: &[&Path]) -> Result<BTreeMap<DirectoryIdentity, PlannedDirectory>> {
    use std::os::unix::fs::MetadataExt as _;

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
                    let identity = (metadata.dev(), metadata.ino());
                    nearest = Some(identity);
                    plan.entry(identity).or_insert_with(|| PlannedDirectory {
                        path: current.clone(),
                        mode: LockMode::Intent,
                    });
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
        plan.get_mut(&nearest)
            .expect("nearest existing directory was inserted")
            .mode = LockMode::Exclusive;
    }
    Ok(plan)
}

#[cfg(all(test, unix))]
fn planned_mode(
    plan: &BTreeMap<DirectoryIdentity, PlannedDirectory>,
    path: &Path,
) -> Option<LockMode> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = std::fs::metadata(path).ok()?;
    plan.get(&(metadata.dev(), metadata.ino()))
        .map(|entry| entry.mode)
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
fn windows_mutex_name(path: &Path) -> Result<String> {
    use std::{os::windows::ffi::OsStrExt as _, ptr};

    use anyhow::Context as _;
    use sha2::{Digest as _, Sha256};
    use windows_sys::Win32::Globalization::{
        LCMAP_UPPERCASE, LCMapStringEx, LOCALE_NAME_INVARIANT,
    };

    // Use Windows' invariant UTF-16 case mapping rather than Rust Unicode
    // lowercasing so case-equivalent NTFS spellings select one kernel mutex.
    let source = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let source_len = i32::try_from(source.len()).context("mutation path is too long")?;
    // SAFETY: source is a valid fixed-length UTF-16 slice; the first call asks
    // Windows for the required destination length and writes no output.
    let mapped_len = unsafe {
        LCMapStringEx(
            LOCALE_NAME_INVARIANT,
            LCMAP_UPPERCASE,
            source.as_ptr(),
            source_len,
            ptr::null_mut(),
            0,
            ptr::null(),
            ptr::null(),
            0,
        )
    };
    if mapped_len == 0 {
        return Err(std::io::Error::last_os_error()).context("failed to case-map mutation path");
    }
    let mut mapped = vec![0_u16; mapped_len as usize];
    // SAFETY: destination has exactly the length returned by the sizing call.
    let written = unsafe {
        LCMapStringEx(
            LOCALE_NAME_INVARIANT,
            LCMAP_UPPERCASE,
            source.as_ptr(),
            source_len,
            mapped.as_mut_ptr(),
            mapped_len,
            ptr::null(),
            ptr::null(),
            0,
        )
    };
    if written != mapped_len {
        return Err(std::io::Error::last_os_error()).context("failed to case-map mutation path");
    }

    let mut digest = Sha256::new();
    for code_unit in mapped {
        digest.update(code_unit.to_le_bytes());
    }
    Ok(format!(
        "Global\\abird-dotlink-path-{}",
        hex::encode(digest.finalize())
    ))
}

#[cfg(windows)]
fn windows_lock_plan(
    paths: &[&Path],
) -> Result<std::collections::BTreeMap<String, std::path::PathBuf>> {
    let mut targets = std::collections::BTreeMap::new();
    for path in paths {
        targets
            .entry(windows_mutex_name(path)?)
            .or_insert_with(|| path.to_path_buf());
    }
    Ok(targets)
}

#[cfg(windows)]
impl WindowsMutationLock {
    async fn acquire(path: &Path, identity: String, deadline: Instant) -> Result<Self> {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
        };
        let path_label = path.display().to_string();
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
                        let error = std::io::Error::last_os_error();
                        tracing::warn!(
                            path = ?path_label,
                            %error,
                            "failed releasing Windows mutation mutex"
                        );
                    }
                } else {
                    let reason = if code == WAIT_TIMEOUT {
                        anyhow::anyhow!("another process is mutating {path_label}; retry")
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
    let deadline = Instant::now()
        .checked_add(LOCK_TIMEOUT)
        .expect("30-second lock deadline fits monotonic clock");
    lock_until(paths, deadline).await
}

/// One deadline covers every in-process lock and each OS-level lock, rather
/// than granting another full timeout after waiting behind a local mutation.
async fn lock_until<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
    deadline: Instant,
) -> Result<MutationGuard> {
    let paths = paths.into_iter().collect::<Vec<_>>();
    let targets = paths.iter().copied().collect::<BTreeSet<_>>();
    let mut guards = Vec::with_capacity(targets.len());
    for target in targets {
        let guard = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            path_lock(target).lock_owned(),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "another mutation is holding {}; retry when it finishes",
                target.display()
            )
        })?;
        guards.push(guard);
    }

    #[cfg(unix)]
    let mut unix_targets = Vec::new();
    #[cfg(unix)]
    for (identity, path) in unix_target_plan(&paths)? {
        let guard = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            UNIX_TARGET_LOCKS
                .get_or_init(Default::default)
                .get(&identity)
                .lock_owned(),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "another mutation is holding {}; retry when it finishes",
                path.display()
            )
        })?;
        unix_targets.push(guard);
    }

    #[cfg(unix)]
    let mut directories = Vec::new();
    #[cfg(unix)]
    for (identity, planned) in lock_plan(&paths)? {
        directories
            .push(DirectoryLock::acquire(&planned.path, identity, planned.mode, deadline).await?);
    }

    #[cfg(windows)]
    let mut windows = Vec::new();
    #[cfg(windows)]
    for (identity, path) in windows_lock_plan(&paths)? {
        windows.push(WindowsMutationLock::acquire(&path, identity, deadline).await?);
    }

    Ok(MutationGuard {
        _paths: guards,
        #[cfg(unix)]
        _targets: unix_targets,
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
            windows_mutex_name(Path::new("C:\\Repo\\DATA.TXT")).unwrap(),
            windows_mutex_name(Path::new("c:\\repo\\data.txt")).unwrap()
        );
        // Greek final sigma is *not* a case pair in the invariant upcase table
        // Windows and NTFS share, so use Cyrillic, which every table folds.
        assert_eq!(
            windows_mutex_name(Path::new("C:\\Repo\\Д.txt")).unwrap(),
            windows_mutex_name(Path::new("c:\\repo\\д.TXT")).unwrap()
        );
        assert_ne!(
            windows_mutex_name(Path::new("C:\\Repo\\a.txt")).unwrap(),
            windows_mutex_name(Path::new("C:\\Repo\\b.txt")).unwrap()
        );
        let plan = windows_lock_plan(&[
            Path::new("C:\\Repo\\Д.txt"),
            Path::new("c:\\repo\\д.TXT"),
            Path::new("C:\\Repo\\other.txt"),
        ])
        .unwrap();
        assert_eq!(plan.len(), 2);
    }

    #[tokio::test]
    async fn in_process_mutation_wait_respects_the_shared_deadline() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap().join("held.txt");
        let guard = lock([path.as_path()]).await.unwrap();
        let deadline = Instant::now() + Duration::from_millis(40);
        let now = Instant::now();
        let error = lock_until([path.as_path()], deadline)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("another mutation is holding"), "{error}");
        assert!(now.elapsed() < Duration::from_secs(2));
        drop(guard);
        // A failed multi-path acquisition releases previously acquired keys.
        let a = root.path().join("a.txt");
        let b = root.path().join("b.txt");
        let held_b = lock([b.as_path()]).await.unwrap();
        assert!(
            lock_until(
                [a.as_path(), b.as_path()],
                Instant::now() + Duration::from_millis(40)
            )
            .await
            .is_err()
        );
        let a_guard = lock_until([a.as_path()], Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        drop(a_guard);
        drop(held_b);
        let both = lock_until(
            [a.as_path(), b.as_path()],
            Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap();
        drop(both);
    }

    #[test]
    fn path_lock_registry_keeps_only_weak_references() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("transient.txt");
        let lock = path_lock(&path);
        let weak = Arc::downgrade(&lock);
        assert!(Arc::ptr_eq(&lock, &path_lock(&path)));
        drop(lock);
        assert!(weak.upgrade().is_none(), "idle locks must be reclaimable");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn in_process_aliases_of_one_directory_entry_share_the_target_lock() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        let direct = root.join("file.txt");
        let alias = root.join("sub").join("..").join("file.txt");
        assert_ne!(direct, alias);
        assert_eq!(
            unix_target_key(&direct).unwrap(),
            unix_target_key(&alias).unwrap()
        );

        let held = lock([direct.as_path()]).await.unwrap();
        let error = lock_until(
            [alias.as_path()],
            Instant::now() + Duration::from_millis(40),
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("another mutation is holding"), "{error}");
        drop(held);
        lock_until([alias.as_path()], Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn old_hash_shard_collisions_do_not_block_distinct_mutation_paths() {
        use std::hash::{DefaultHasher, Hash, Hasher};
        use tokio::sync::oneshot;
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let a = root.join("a.txt");
        let old_shard = |path: &Path| {
            let mut hasher = DefaultHasher::new();
            path.hash(&mut hasher);
            hasher.finish() % 256
        };
        let b = (0..10_000)
            .map(|n| root.join(format!("different-{n}.txt")))
            .find(|path| path != &a && old_shard(path) == old_shard(&a))
            .expect("10,000 candidates contain a colliding shard");
        let held = lock([a.as_path()]).await.unwrap();
        let (send, recv) = oneshot::channel();
        let worker = tokio::spawn(async move {
            let _different = lock([b.as_path()]).await.unwrap();
            let _ = send.send(());
        });
        tokio::time::timeout(Duration::from_secs(3), recv)
            .await
            .expect("different files must never share an in-process path lock")
            .unwrap();
        worker.await.unwrap();
        drop(held);
    }

    #[cfg(unix)]
    #[test]
    fn parent_creation_is_locked_at_the_nearest_existing_ancestor() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().canonicalize().unwrap();
        let target = parent.join("new/child/data.txt");
        let plan = lock_plan(&[target.as_path()]).unwrap();
        assert_eq!(planned_mode(&plan, &parent), Some(LockMode::Exclusive));
        assert!(planned_mode(&plan, &parent.join("new")).is_none());
        std::fs::create_dir_all(parent.join("new/child")).unwrap();
        let plan = lock_plan(&[target.as_path()]).unwrap();
        assert_eq!(planned_mode(&plan, &parent), Some(LockMode::Intent));
        assert_eq!(
            planned_mode(&plan, &parent.join("new/child")),
            Some(LockMode::Exclusive)
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
    async fn directory_lock_rejects_an_inode_changed_since_planning() {
        use std::os::unix::fs::MetadataExt as _;
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let metadata = std::fs::metadata(&a).unwrap();
        let expected = (metadata.dev(), metadata.ino());
        let error = DirectoryLock::acquire(
            &b,
            expected,
            LockMode::Exclusive,
            Instant::now() + Duration::from_secs(1),
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("changed while locking"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn one_process_reuses_the_held_directory_descriptor() {
        use std::os::unix::fs::MetadataExt as _;
        let root = tempfile::tempdir().unwrap();
        let metadata = std::fs::metadata(root.path()).unwrap();
        let identity = (metadata.dev(), metadata.ino());
        let deadline = Instant::now() + LOCK_TIMEOUT;
        let a = DirectoryLock::acquire(root.path(), identity, LockMode::Exclusive, deadline)
            .await
            .unwrap();
        let b = DirectoryLock::acquire(root.path(), identity, LockMode::Exclusive, deadline)
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }
}
