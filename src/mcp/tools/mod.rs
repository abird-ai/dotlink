//! The tool surface: one module per tool family plus the primitives they share.
//!
//! Everything here is a descendant of `crate::mcp`, so a tool sees the machine's
//! access policy and pinned descriptors without widening either. A new tool
//! adds one module, one router, one line in the capability table, and nothing
//! else: the walk, glob grammar, limits, truncation markers and commit path are
//! shared rather than copied.

pub(crate) mod binary;
pub(crate) mod edit;
pub(crate) mod grep;
pub(crate) mod ls;
pub(crate) mod patch;
pub(crate) mod shell;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod text;

use std::{
    collections::BinaryHeap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use anyhow::{Context, Result, anyhow, bail};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rmcp::model::{CallToolResult, ContentBlock};
use tokio::fs;

use super::LocalMachine;

/// Shared bounds. Per-tool bounds live with the tool that owns them.
pub(crate) const GITIGNORE_MAX_BYTES: usize = 64 * 1024;
pub(crate) const GITIGNORE_MAX_LINES: usize = 1024;
/// Aggregate independent of grep's content budget: ignore files are also I/O
/// and matcher-construction work, not free side effects of directory walking.
const MAX_IGNORE_SCAN_BYTES: usize = 4 * 1024 * 1024;
const MAX_IGNORE_SCAN_FILES: usize = 256;

#[derive(Default)]
struct IgnoreWork {
    bytes: usize,
    files: usize,
    incomplete: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum IgnoreSizeVerification {
    Stable,
    PerFileLimit,
    AggregateLimit,
    Changed,
}

fn verify_ignore_size(
    declared: u64,
    observed: u64,
    aggregate_remaining: usize,
) -> IgnoreSizeVerification {
    if observed > GITIGNORE_MAX_BYTES as u64 {
        IgnoreSizeVerification::PerFileLimit
    } else if observed > aggregate_remaining as u64 {
        IgnoreSizeVerification::AggregateLimit
    } else if observed != declared {
        IgnoreSizeVerification::Changed
    } else {
        IgnoreSizeVerification::Stable
    }
}

impl IgnoreWork {
    /// Charge a non-absent, policy-allowed ignore-file attempt before reading.
    fn begin_file(&mut self) -> Result<usize> {
        if self.files >= MAX_IGNORE_SCAN_FILES {
            bail!(
                "gitignore search work exceeded {MAX_IGNORE_SCAN_FILES} files; narrow path or disable gitignore"
            );
        }
        self.files += 1;
        Ok(MAX_IGNORE_SCAN_BYTES.saturating_sub(self.bytes))
    }

    /// Account for bytes yielded before success or failure. The ceiling check
    /// is an invariant backstop; bounded readers should never trigger it.
    fn finish_read(&mut self, bytes: usize, succeeded: bool) -> Result<bool> {
        let remaining = MAX_IGNORE_SCAN_BYTES.saturating_sub(self.bytes);
        self.bytes += bytes.min(remaining);
        if !succeeded {
            self.incomplete = true;
        }
        if bytes > remaining {
            bail!(
                "gitignore search work exceeded {MAX_IGNORE_SCAN_BYTES} bytes; narrow path or disable gitignore"
            );
        }
        Ok(succeeded)
    }
}

/// Per-call discovery work, not a cap on model-directed concurrency.
/// A truncated traversal is explicitly marked partial: top-K ordering is
/// global only when the walk completes.
pub(crate) const MAX_WALK_VISITS: usize = 100_000;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct WalkProgress {
    pub visited: usize,
    /// Requested discovery exhausted its entry budget.
    pub incomplete: bool,
    /// Requested descendants could not be opened, listed, or inspected.
    pub traversal_incomplete: bool,
    /// An opt-in ignore file could not be applied.
    pub ignore_incomplete: bool,
}

impl WalkProgress {
    pub fn scan_incomplete(&self) -> bool {
        self.incomplete || self.traversal_incomplete || self.ignore_incomplete
    }

    /// Every independent cause in compatibility-priority order. New causes can
    /// be appended without changing the legacy singular reason.
    pub fn incomplete_reasons(&self) -> Vec<&'static str> {
        let mut reasons = Vec::with_capacity(3);
        if self.incomplete {
            reasons.push("directory_budget");
        }
        if self.traversal_incomplete {
            reasons.push("traversal_error");
        }
        if self.ignore_incomplete {
            reasons.push("ignore_rules");
        }
        reasons
    }

    pub fn incomplete_reason(&self) -> Option<&'static str> {
        if self.incomplete {
            Some("directory_budget")
        } else if self.traversal_incomplete {
            Some("traversal_error")
        } else if self.ignore_incomplete {
            Some("ignore_rules")
        } else {
            None
        }
    }
}

/// Text tools accept UTF-8 without binary control bytes. Keep this predicate
/// shared by paged reads, search, snapshots, and text mutations. Binary tools
/// deliberately have no such restriction.
pub(crate) fn has_binary_control(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .any(|byte| matches!(byte, 0..=8 | 11..=12 | 14..=31 | 127))
}

pub(crate) fn text_bytes(bytes: &[u8]) -> Option<&str> {
    if has_binary_control(bytes) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

pub(crate) fn require_text_bytes<'a>(bytes: &'a [u8], hint: &str) -> Result<&'a str> {
    text_bytes(bytes).ok_or_else(|| anyhow!("binary content; use {hint}"))
}

fn dot() -> String {
    ".".to_owned()
}

fn ok_text(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text.into())])
}

fn tool_error(error: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.to_string())])
}

/// Client-controlled limit under a policy ceiling: the caller picks any value up
/// to the configured ceiling, which is also the fallback when omitted.
pub(crate) fn resolve_capped<T>(requested: Option<T>, ceiling: T) -> T
where
    T: Ord + From<u8> + Copy,
{
    let ceiling = ceiling.max(T::from(1));
    requested.unwrap_or(ceiling).clamp(T::from(1), ceiling)
}

/// One glob grammar for every tool: `globset` with `literal_separator`, so
/// `*`/`?`/`[...]` never cross `/` and `**` is the only recursive form.
/// `{a,b}` alternation comes from the same builder. Candidates are always a
/// path already held in memory; nothing here touches the filesystem.
pub(crate) struct GlobFilter {
    matcher: globset::GlobMatcher,
}

impl GlobFilter {
    pub(crate) fn new(pattern: &str) -> Result<Self> {
        let matcher = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .with_context(|| format!("invalid glob pattern: {pattern}"))?
            .compile_matcher();
        Ok(Self { matcher })
    }

    /// Empty patterns are absent filters, not errors: a model that passes `""`
    /// means "no filter", and an empty glob matches everything anyway.
    pub(crate) fn optional(pattern: Option<&str>) -> Result<Option<Self>> {
        pattern
            .filter(|pattern| !pattern.is_empty())
            .map(Self::new)
            .transpose()
    }

    fn matches(&self, candidate: &Path) -> bool {
        self.matcher.is_match(candidate)
    }

    /// Match an entry by its path relative to the search root, or by its bare
    /// name: `*.rs` stays useful below the root, `src/**/*.rs` addresses a
    /// subtree. One rule, so no two tools can drift.
    pub(crate) fn matches_entry(&self, relative: &Path) -> bool {
        self.matches(relative)
            || relative
                .file_name()
                .is_some_and(|name| self.matches(Path::new(name)))
    }
}

/// Pinned handle of a directory, plus the resolve mask it was pinned with.
#[cfg(any(target_os = "linux", target_os = "macos"))]
type DirHandle = Arc<DirHandleInner>;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
pub(crate) struct DirHandleInner {
    fd: std::os::fd::OwnedFd,
    resolve: u64,
}

#[cfg(windows)]
type DirHandle = Arc<crate::windows_fs::PinnedDirectory>;

/// One directory entry produced by the shared walk.
pub(crate) struct WalkedEntry {
    /// Logical, policy-checked path, used for denies, display and recursion.
    pub(crate) path: PathBuf,
    /// `path` relative to the walk root; the candidate for glob matching.
    pub(crate) relative: PathBuf,
    /// `file`, `dir`, `symlink` or `other`.
    pub(crate) kind: &'static str,
    pub(crate) size: u64,
    pub(crate) mtime: Option<SystemTime>,
}

/// Filters applied before an entry reaches the tool: opt-in hidden skipping and
/// opt-in `.gitignore` handling.
#[derive(Default, Clone, Copy)]
pub(crate) struct WalkFilter {
    pub(crate) skip_hidden: bool,
    pub(crate) gitignore: bool,
}

fn starts_with_dot(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

/// Directories that cannot be enumerated are skipped, like `rg`: a permission
/// wall or a directory that vanished mid-walk is not a tool failure. Policy
/// denies never reach here — they are applied before a directory is queued.
fn skippable_io_error(error: &std::io::Error) -> bool {
    // These errno values vary between Linux and macOS; the libc target owns
    // them. Windows keeps the portable error classifications below.
    let portable = matches!(
        error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::NotADirectory
    );
    #[cfg(unix)]
    {
        portable || matches!(error.raw_os_error(), Some(libc::EXDEV) | Some(libc::ELOOP))
    }
    #[cfg(windows)]
    {
        portable
    }
}

fn skippable_directory_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(skippable_io_error)
}

/// Deepest matcher first; the first decisive rule wins, which is git's order.
fn is_gitignored(ignores: &[Arc<Gitignore>], path: &Path, is_dir: bool) -> bool {
    for ignore in ignores.iter().rev() {
        match ignore.matched(path, is_dir) {
            ignore::Match::Ignore(_) => return true,
            ignore::Match::Whitelist(_) => return false,
            ignore::Match::None => {}
        }
    }
    false
}

/// A directory queued for descent: the pinned parent handle plus the single
/// name to open under it. Opening happens when the directory is popped, so an
/// unreadable child is skipped at the same place the `read_dir` failure is.
struct PendingDir {
    parent: Option<DirHandle>,
    name: OsString,
    path: PathBuf,
    remaining: usize,
    ignores: Vec<Arc<Gitignore>>,
}

impl LocalMachine {
    /// Pin the caller-named root. A root that cannot be opened is a tool error,
    /// never a silently empty result.
    fn pin_root_directory(&self, root: &Path) -> Result<Option<DirHandle>> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let resolve = self.secure_fs.read_resolve(root)?;
            Ok(Some(Arc::new(DirHandleInner {
                fd: self.secure_fs.open_directory(root)?.into(),
                resolve,
            })))
        }
        #[cfg(windows)]
        {
            Ok(Some(Arc::new(crate::windows_fs::PinnedDirectory::new(
                root, false,
            )?)))
        }
    }

    /// Pin a child directory from its parent handle, so descent never resolves a
    /// path a second time and never relaxes the parent's resolve mask.
    fn pin_child_directory(
        &self,
        parent: Option<&DirHandle>,
        name: &OsStr,
        path: &Path,
    ) -> Result<Option<DirHandle>> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let Some(parent) = parent else {
                return self.pin_root_directory(path);
            };
            Ok(Some(Arc::new(DirHandleInner {
                fd: self
                    .secure_fs
                    .open_child_dir(&parent.fd, name, parent.resolve)?
                    .into(),
                resolve: parent.resolve,
            })))
        }
        #[cfg(windows)]
        {
            let _ = (parent, name);
            Ok(Some(Arc::new(crate::windows_fs::PinnedDirectory::new(
                path, false,
            )?)))
        }
    }

    /// Enumerate exactly the pinned directory. Linux has no `read_dir` over a
    /// descriptor, so the pinned FD is reached through procfs; if that is not
    /// available the walk fails closed rather than reopening a pathname that a
    /// concurrent rename could have redirected.
    async fn read_directory(&self, handle: Option<&DirHandle>, path: &Path) -> Result<fs::ReadDir> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let Some(handle) = handle else {
                bail!(
                    "descriptor-rooted enumeration requires a pinned directory handle for {}",
                    path.display()
                );
            };
            #[cfg(target_os = "linux")]
            let fd_root = "/proc/self/fd";
            #[cfg(target_os = "macos")]
            let fd_root = "/dev/fd";
            let pinned = PathBuf::from(format!(
                "{}/{}",
                fd_root,
                std::os::fd::AsRawFd::as_raw_fd(&handle.fd)
            ));
            let reader = fs::read_dir(&pinned).await.with_context(|| {
                format!(
                    "descriptor-rooted enumeration needs an OS descriptor filesystem; cannot list {}",
                    path.display()
                )
            })?;
            Ok(reader)
        }
        #[cfg(windows)]
        {
            if handle.is_none() {
                bail!("Windows native enumeration requires a pinned directory");
            }
            // All ancestors and this directory remain open without delete
            // sharing for the entire walk; the path cannot be redirected.
            Ok(fs::read_dir(path).await?)
        }
    }

    /// Distinguish an absent .gitignore from an unreadable/unsafe one while
    /// staying beneath the directory's pinned descriptor. Other child reads
    /// intentionally have a lossy Option API; ignore completeness needs errors.
    async fn open_ignore_file(
        &self,
        handle: Option<&DirHandle>,
        path: &Path,
    ) -> Result<Option<fs::File>> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(parent) = handle {
            return match self.secure_fs.open_child_read(
                &parent.fd,
                OsStr::new(".gitignore"),
                parent.resolve,
            ) {
                Ok(file) => Ok(Some(fs::File::from_std(file))),
                Err(error)
                    if error
                        .chain()
                        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
                        .any(|cause| cause.kind() == std::io::ErrorKind::NotFound) =>
                {
                    Ok(None)
                }
                Err(error) => Err(error),
            };
        }
        // Windows parent directories are pinned through their handles; the
        // platform's open_read_file refuses reparse points. Tests also use
        // this fallback when no handle is passed explicitly.
        #[cfg(windows)]
        let _pinned_parent = handle;
        match self.open_read_file(path).await {
            Ok(file) => Ok(Some(file)),
            Err(error)
                if error
                    .chain()
                    .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
                    .any(|cause| cause.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Read exactly the descriptor's declared bytes without probing past either
    /// I/O ceiling. A short read or read error is incomplete, and every byte
    /// obtained before that uncertainty is still charged.
    async fn read_ignore_bytes<R: tokio::io::AsyncRead + Unpin>(
        reader: R,
        expected_len: usize,
        work: &mut IgnoreWork,
    ) -> Result<Option<Vec<u8>>> {
        let remaining = work.begin_file()?;
        if expected_len > GITIGNORE_MAX_BYTES {
            bail!("file exceeds the {GITIGNORE_MAX_BYTES} byte .gitignore limit");
        }
        if expected_len > remaining {
            bail!(
                "gitignore search work exceeded {MAX_IGNORE_SCAN_BYTES} bytes; narrow path or disable gitignore"
            );
        }
        let mut bytes = Vec::with_capacity(expected_len.min(8 * 1024));
        let result = reader
            .take(expected_len as u64)
            .read_to_end(&mut bytes)
            .await;
        let complete = result.is_ok() && bytes.len() == expected_len;
        if !work.finish_read(bytes.len(), complete)? {
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    /// Read this directory's `.gitignore` through the pinned handle and turn it
    /// into a matcher rooted at its own directory. Bounded at 64 KiB and 1024
    /// lines; an invalid pattern fails the whole call (fail closed).
    async fn load_gitignore(
        &self,
        dir: &Path,
        handle: Option<&DirHandle>,
        work: &mut IgnoreWork,
    ) -> Result<Option<Gitignore>> {
        let path = dir.join(".gitignore");
        // The walk drops denied entries, but this open is separate: a deny rule
        // or protected path on the ignore file itself must still win.
        if self.config.access.denied_read(&path) || self.ensure_not_protected(&path).is_err() {
            // A denied ignore file is never read. Conservatively flag the
            // resulting search as partial rather than asserting full ignores.
            work.incomplete = true;
            return Ok(None);
        }
        let mut file = match self.open_ignore_file(handle, &path).await {
            Ok(Some(file)) => file,
            Ok(None) => return Ok(None),
            Err(_) => {
                // The path was not absent and policy allowed the attempt. It
                // therefore consumes one file even though no bytes arrived.
                work.begin_file()?;
                work.incomplete = true;
                return Ok(None);
            }
        };
        let before = match file.metadata().await {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) | Err(_) => {
                work.begin_file()?;
                work.incomplete = true;
                return Ok(None);
            }
        };
        if before.len() > GITIGNORE_MAX_BYTES as u64 {
            work.begin_file()?;
            bail!(
                "{} exceeds the {} byte .gitignore limit",
                path.display(),
                GITIGNORE_MAX_BYTES
            );
        }
        let remaining = MAX_IGNORE_SCAN_BYTES.saturating_sub(work.bytes);
        let expected_len = before.len() as usize;
        let Some(bytes) = Self::read_ignore_bytes(&mut file, expected_len, work).await? else {
            return Ok(None);
        };
        let after = match file.metadata().await {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) | Err(_) => {
                work.incomplete = true;
                return Ok(None);
            }
        };
        match verify_ignore_size(before.len(), after.len(), remaining) {
            IgnoreSizeVerification::PerFileLimit => {
                bail!(
                    "{} exceeds the {} byte .gitignore limit",
                    path.display(),
                    GITIGNORE_MAX_BYTES
                );
            }
            IgnoreSizeVerification::AggregateLimit => {
                bail!(
                    "gitignore search work exceeded {MAX_IGNORE_SCAN_BYTES} bytes; narrow path or disable gitignore"
                );
            }
            IgnoreSizeVerification::Changed => {
                work.incomplete = true;
                return Ok(None);
            }
            IgnoreSizeVerification::Stable => {}
        }
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!(".gitignore is not UTF-8: {}", path.display()))?;
        if text.lines().count() > GITIGNORE_MAX_LINES {
            bail!(
                "{} exceeds the {} line .gitignore limit",
                path.display(),
                GITIGNORE_MAX_LINES
            );
        }
        let mut builder = GitignoreBuilder::new(dir);
        for (index, line) in text.lines().enumerate() {
            builder
                .add_line(Some(path.clone()), line)
                .map_err(|error| {
                    anyhow!(
                        "invalid .gitignore pattern in {} at line {}: {error}",
                        path.display(),
                        index + 1
                    )
                })?;
        }
        Ok(Some(builder.build().with_context(|| {
            format!("invalid .gitignore in {}", path.display())
        })?))
    }
}

use tokio::io::AsyncReadExt;

impl LocalMachine {
    /// Descriptor-rooted directory walk shared by `ls` and `grep`.
    ///
    /// Authorization is decided once, for the root the caller named; descent
    /// and `.gitignore` reads travel through pinned directory handles. File
    /// contents are re-opened by path through the descriptor-rooted reader under
    /// the pinned grant root, one file at a time, so a large tree cannot exhaust
    /// descriptors; a directory rename inside the grant can change which inode
    /// that path names, but never escapes the grant. Denies and protected paths
    /// are dropped before the caller sees an entry. Returning `false` from
    /// `on_entry` stops the walk.
    #[cfg(test)]
    pub(crate) async fn walk_entries(
        &self,
        root: &Path,
        depth: usize,
        filter: WalkFilter,
        on_entry: impl FnMut(WalkedEntry) -> bool,
    ) -> Result<()> {
        self.walk_entries_bounded(root, depth, filter, usize::MAX, on_entry)
            .await?;
        Ok(())
    }

    /// One shared visitor budget across listing/searching. The limit is counted
    /// *before* entry filters so a tree full of denied or hidden files still
    /// cannot force an unbounded scan. No false claim of global order on a
    /// partial traversal.
    pub(crate) async fn walk_entries_bounded(
        &self,
        root: &Path,
        depth: usize,
        filter: WalkFilter,
        max_visits: usize,
        mut on_entry: impl FnMut(WalkedEntry) -> bool,
    ) -> Result<WalkProgress> {
        let mut progress = WalkProgress::default();
        let mut ignore_work = IgnoreWork::default();
        let mut pending: Vec<PendingDir> = vec![PendingDir {
            parent: None,
            name: OsString::new(),
            path: root.to_path_buf(),
            remaining: depth.max(1),
            ignores: Vec::new(),
        }];

        while let Some(PendingDir {
            parent,
            name,
            path: dir,
            remaining,
            mut ignores,
        }) = pending.pop()
        {
            let is_root = dir == root;
            let handle = match self.pin_child_directory(parent.as_ref(), &name, &dir) {
                Ok(handle) => handle,
                Err(error) if !is_root && skippable_directory_error(&error) => {
                    progress.traversal_incomplete = true;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let mut reader = match self.read_directory(handle.as_ref(), &dir).await {
                Ok(reader) => reader,
                Err(error) if !is_root && skippable_directory_error(&error) => {
                    progress.traversal_incomplete = true;
                    continue;
                }
                Err(error) => return Err(error),
            };
            // Only a directory we can actually enumerate contributes ignore
            // rules, so an unreadable subtree cannot fail the whole search.
            if filter.gitignore
                && let Some(ignore) = self
                    .load_gitignore(&dir, handle.as_ref(), &mut ignore_work)
                    .await?
            {
                ignores.push(Arc::new(ignore));
            }

            loop {
                let entry = match reader.next_entry().await {
                    Ok(Some(entry)) => entry,
                    Ok(None) => break,
                    // Stop this directory, keep the rest of the walk.
                    Err(error) if skippable_io_error(&error) => {
                        progress.traversal_incomplete = true;
                        break;
                    }
                    Err(error) => return Err(error.into()),
                };
                if progress.visited >= max_visits {
                    progress.incomplete = true;
                    progress.ignore_incomplete = ignore_work.incomplete;
                    return Ok(progress);
                }
                progress.visited += 1;
                let name = entry.file_name();
                // In-flight staged bytes are not a file the caller can see.
                if crate::staging::is_staging_temporary(&name) {
                    continue;
                }
                let path = dir.join(&name);

                if self.config.access.denied_read(&path)
                    || self.ensure_not_protected(&path).is_err()
                {
                    continue;
                }
                if filter.skip_hidden && starts_with_dot(&name) {
                    continue;
                }

                // Inspect the already opened directory rather than reopening the
                // containing path. Entries may disappear while other tools work
                // concurrently; skip them rather than failing the listing.
                let metadata = match fs::symlink_metadata(entry.path()).await {
                    Ok(metadata) => metadata,
                    Err(error) if skippable_io_error(&error) => {
                        progress.traversal_incomplete = true;
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                };
                let file_type = metadata.file_type();
                let kind = if file_type.is_symlink() {
                    "symlink"
                } else if file_type.is_dir() {
                    "dir"
                } else if file_type.is_file() {
                    "file"
                } else {
                    "other"
                };

                // Deeper `.gitignore` files win, and a whitelist stops the
                // search: that is git's precedence order.
                if !ignores.is_empty() && is_gitignored(&ignores, &path, file_type.is_dir()) {
                    continue;
                }

                let descend =
                    remaining > 1 && file_type.is_dir() && self.config.access.can_read(&path);
                let walked = WalkedEntry {
                    relative: path.strip_prefix(root).unwrap_or(&path).to_path_buf(),
                    path: path.clone(),
                    kind,
                    size: metadata.len(),
                    mtime: metadata.modified().ok(),
                };
                if !on_entry(walked) {
                    progress.ignore_incomplete = ignore_work.incomplete;
                    return Ok(progress);
                }
                if descend {
                    pending.push(PendingDir {
                        parent: handle.clone(),
                        name,
                        path,
                        remaining: remaining - 1,
                        ignores: ignores.clone(),
                    });
                }
            }
        }

        progress.ignore_incomplete = ignore_work.incomplete;
        Ok(progress)
    }
}

/// Byte offsets of every occurrence of `needle` in `haystack`.
pub(crate) fn match_indices(haystack: &str, needle: &str) -> Vec<usize> {
    haystack.match_indices(needle).map(|(at, _)| at).collect()
}

/// 1-based line number of a byte offset.
pub(crate) fn line_number(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

pub(crate) fn line_count(text: &str) -> usize {
    text.lines().count()
}

/// `read` numbers lines as `N\tline`; an `old_text` copied from that output
/// starts with digits and a tab.
pub(crate) fn has_line_number_prefix(text: &str) -> bool {
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && text[digits..].starts_with('\t')
}

/// Every capped result says so once, in one shape: `… truncated at <what>`.
/// JSON tools carry `"truncated": true` instead, so a machine reader never has
/// to parse prose.
pub(crate) fn marker_lines(last: usize) -> String {
    format!("… truncated at line {last}")
}

pub(crate) fn marker_hits(hits: usize) -> String {
    format!("… truncated at {hits} hits")
}

pub(crate) fn marker_files(files: usize) -> String {
    format!("… truncated at {files} files")
}

pub(crate) fn marker_bytes(bytes: usize) -> String {
    format!("… truncated at {bytes} bytes")
}

/// Retain the globally best K entries in bounded memory. The heap's root is
/// always the worst retained item, independent of discovery order.
/// Returns true when more than K candidates have been examined.
pub(crate) fn retain_best<T: Ord>(heap: &mut BinaryHeap<T>, candidate: T, limit: usize) -> bool {
    if heap.len() < limit {
        heap.push(candidate);
        return false;
    }
    if heap.peek().is_some_and(|worst| candidate < *worst) {
        heap.pop();
        heap.push(candidate);
    }
    true
}

/// Keep the longest prefix of `records` whose rendered JSON fits the ceiling.
/// The caller's renderer is the exact reply shape, including every derived
/// field, so the measured size and the returned size cannot drift. Rendered
/// length grows with the prefix, so one binary search finds the cut in
/// `O(log n)` renders instead of re-serializing once per dropped record.
pub(crate) fn trim_to_ceiling<T>(
    mut records: Vec<T>,
    ceiling: usize,
    truncated: bool,
    mut render: impl FnMut(&[T], bool) -> String,
) -> (Vec<T>, bool) {
    if render(&records, truncated).len() <= ceiling {
        return (records, truncated);
    }
    let (mut low, mut high) = (0_usize, records.len());
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if render(&records[..middle], true).len() <= ceiling {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    records.truncate(low);
    (records, true)
}

/// The bytes a text tool planned against.
///
/// Planning, verification and writing are three separate steps over one value,
/// so no tool can quietly skip one of them: `read` once, `commit` re-reads and
/// refuses to write when the file moved on.
pub(crate) struct Snapshot {
    pub(crate) path: PathBuf,
    pub(crate) display: String,
    pub(crate) bytes: Vec<u8>,
    /// Ceiling for the planning read and the verification re-read. Text tools
    /// use the profile read limit; `bedit` uses the patchable-file size.
    limit: usize,
}

impl Snapshot {
    pub(crate) async fn read(
        machine: &LocalMachine,
        path: PathBuf,
        display: impl Into<String>,
    ) -> Result<Self> {
        Self::read_within(machine, path, display, machine.config.max_read_bytes).await
    }

    /// Read a snapshot under an explicit size ceiling, so a caller with a tighter
    /// or looser bound than the profile read limit does not read more than it
    /// can accept.
    pub(crate) async fn read_within(
        machine: &LocalMachine,
        path: PathBuf,
        display: impl Into<String>,
        limit: usize,
    ) -> Result<Self> {
        let mut file = machine.open_edit_file(&path).await?;
        let bytes = LocalMachine::read_bounded(&mut file, limit).await?;
        Ok(Self {
            path,
            display: display.into(),
            bytes,
            limit,
        })
    }

    pub(crate) fn display(&self) -> &str {
        &self.display
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The snapshot as UTF-8, or the hint that sends the caller to `bedit`.
    pub(crate) fn text(&self) -> Result<&str> {
        require_text_bytes(&self.bytes, "bedit")
            .map_err(|_| anyhow!("{}: binary file; use bedit", self.display))
    }

    /// Confirm the file still holds these bytes, and hand back the token
    /// `Verified::commit` requires. Verification cannot be skipped, and it costs
    /// exactly one read: the type system enforces the plan → verify → write order
    /// for every existing-file text update.
    pub(crate) async fn verify(self, machine: &LocalMachine, verb: &str) -> Result<Verified> {
        let mut file = machine.open_edit_file(&self.path).await?;
        let current = LocalMachine::read_bounded(&mut file, self.limit).await?;
        if current != self.bytes {
            // The caller states the consequence: an atomic tool wrote nothing,
            // `best_effort` skipped this file and kept the rest.
            bail!("{} changed while {verb}", self.display);
        }
        Ok(Verified {
            path: self.path,
            display: self.display,
            bytes: self.bytes,
        })
    }
}

/// A snapshot that was re-read and still matches. The only way to obtain one is
/// `Snapshot::verify`; an update can only be staged through this token, so the
/// plan → verify → write order is not optional. Creates (`write`, `bwrite`,
/// `*** Add File`) have no prior content to verify and stage directly.
#[derive(Debug)]
pub(crate) struct Verified {
    path: PathBuf,
    display: String,
    bytes: Vec<u8>,
}

impl Verified {
    pub(crate) fn display(&self) -> &str {
        &self.display
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Stage the replacement without making it visible. `None` means the file
    /// already holds this content, so nothing is written and no mtime moves.
    /// Callers that write several files stage them all before committing any.
    pub(crate) async fn stage(
        self,
        machine: &LocalMachine,
        text: &[u8],
    ) -> Result<Option<super::PendingWrite>> {
        if text == self.bytes.as_slice() {
            return Ok(None);
        }
        let expected = (self.bytes.len() as u64, super::sha256(&self.bytes));
        Ok(Some(
            machine
                .stage_update_write(&self.path, text, expected)
                .await?,
        ))
    }

    /// Stage and immediately commit one file.
    pub(crate) async fn commit(self, machine: &LocalMachine, text: &[u8]) -> Result<bool> {
        match self.stage(machine, text).await? {
            Some(pending) => machine.commit_write(pending).await.map(|()| true),
            None => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::test_support::test_machine;

    #[cfg(unix)]
    #[test]
    fn skippable_errors_use_the_native_errno_values() {
        use std::io::{Error, ErrorKind};
        assert!(skippable_io_error(&Error::from_raw_os_error(libc::EXDEV)));
        assert!(skippable_io_error(&Error::from_raw_os_error(libc::ELOOP)));
        assert!(skippable_io_error(&Error::from(
            ErrorKind::PermissionDenied
        )));
        assert!(skippable_io_error(&Error::from(ErrorKind::NotFound)));
        assert!(!skippable_io_error(&Error::from(ErrorKind::InvalidData)));
    }

    #[test]
    fn concurrent_walk_incompleteness_retains_every_reason() {
        let progress = WalkProgress {
            visited: 42,
            incomplete: true,
            traversal_incomplete: true,
            ignore_incomplete: true,
        };
        assert!(progress.scan_incomplete());
        assert_eq!(progress.incomplete_reason(), Some("directory_budget"));
        assert_eq!(
            progress.incomplete_reasons(),
            ["directory_budget", "traversal_error", "ignore_rules"]
        );
    }

    #[tokio::test]
    async fn discovery_scan_budget_is_enforced_before_candidate_filters() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for index in 0..12 {
            std::fs::write(root.join(format!("{index:02}.txt")), "").unwrap();
        }
        let machine = test_machine(root.clone());
        let mut reported = 0;
        let progress = machine
            .walk_entries_bounded(&root, 1, WalkFilter::default(), 3, |_| {
                reported += 1;
                true
            })
            .await
            .unwrap();
        assert!(progress.incomplete);
        assert_eq!(progress.visited, 3);
        assert_eq!(reported, 3);
    }

    #[test]
    fn top_k_remains_globally_sorted_with_arbitrary_discovery_order() {
        let mut heap = BinaryHeap::new();
        let mut truncated = false;
        for number in (0..100).rev() {
            truncated |= retain_best(&mut heap, number, 8);
        }
        assert!(truncated);
        assert_eq!(heap.into_sorted_vec(), (0..8).collect::<Vec<_>>());
    }

    #[test]
    fn glob_grammar_covers_stars_classes_alternation_and_separators() {
        let matches = |pattern: &str, candidate: &str| {
            GlobFilter::new(pattern)
                .unwrap()
                .matches(Path::new(candidate))
        };
        assert!(matches("*.rs", "lib.rs"));
        assert!(!matches("*.rs", "src/lib.rs"), "`*` never crosses `/`");
        assert!(matches("src/*.rs", "src/lib.rs"));
        assert!(!matches("src/*.rs", "src/nested/lib.rs"));
        assert!(matches("src/**/*.rs", "src/nested/deep/lib.rs"));
        assert!(matches("src/**/*.rs", "src/lib.rs"));
        assert!(matches("?.md", "a.md"));
        assert!(!matches("?.md", "ab.md"));
        assert!(matches("[ab].md", "a.md"));
        assert!(matches("*.{rs,md}", "notes.md"));
        assert!(!matches("*.{rs,md}", "notes.txt"));
        assert!(GlobFilter::new("[").is_err());
        assert!(GlobFilter::optional(Some("")).unwrap().is_none());
    }

    #[test]
    fn entry_matching_covers_the_relative_path_and_the_bare_name() {
        let filter = GlobFilter::new("*.rs").unwrap();
        assert!(filter.matches_entry(Path::new("src/nested/deep.rs")));
        let subtree = GlobFilter::new("src/**/*.rs").unwrap();
        assert!(subtree.matches_entry(Path::new("src/nested/deep.rs")));
        assert!(!subtree.matches_entry(Path::new("tests/deep.rs")));
    }

    #[test]
    fn markers_are_one_shape() {
        assert_eq!(marker_lines(4096), "… truncated at line 4096");
        assert_eq!(marker_hits(200), "… truncated at 200 hits");
        assert_eq!(marker_files(1024), "… truncated at 1024 files");
        assert_eq!(marker_bytes(50), "… truncated at 50 bytes");
    }

    #[test]
    fn trimming_keeps_the_longest_fitting_prefix_in_logarithmic_renders() {
        let records = (0..1024).map(|index| index.to_string()).collect::<Vec<_>>();
        let render = |kept: &[String], truncated: bool| {
            serde_json::json!({ "files": kept, "truncated": truncated }).to_string()
        };
        // One record renders as `"0"` (plus the comma and the framing). A
        // ceiling of 64 bytes cannot hold the whole array.
        let whole = render(&records, false);
        assert!(whole.len() > 64);

        let mut renders = 0;
        let (kept, truncated) = trim_to_ceiling(records.clone(), 64, false, |kept, truncated| {
            renders += 1;
            render(kept, truncated)
        });
        assert!(truncated);
        assert!(!kept.is_empty() && kept.len() < records.len());
        assert!(render(&kept, truncated).len() <= 64);
        // Binary search: a naive pop loop would render ~1000 times here.
        assert!(renders <= 12, "renders: {renders}");

        // Everything fits: nothing is dropped and the flag is preserved.
        let (kept, truncated) = trim_to_ceiling(records.clone(), 1 << 20, false, render);
        assert_eq!(kept.len(), records.len());
        assert!(!truncated);
        // Nothing fits: the empty reply is still returned with the flag set.
        let (kept, truncated) = trim_to_ceiling(records, 8, false, render);
        assert!(kept.is_empty());
        assert!(truncated);
    }

    #[test]
    fn resolve_capped_keeps_the_ceiling_and_the_floor() {
        assert_eq!(resolve_capped(None, 1_048_576_usize), 1_048_576);
        assert_eq!(resolve_capped(Some(4_096_usize), 1_048_576), 4_096);
        assert_eq!(resolve_capped(Some(8_388_608_usize), 1_048_576), 1_048_576);
        assert_eq!(resolve_capped(Some(0_usize), 1_048_576), 1);
        assert_eq!(resolve_capped::<u64>(None, 0), 1, "a zero ceiling is one");
    }

    /// Walk helper: collect every entry reachable from `root`.
    async fn walk_paths(machine: &LocalMachine, root: &Path, depth: usize) -> Vec<String> {
        let mut paths = Vec::new();
        machine
            .walk_entries(root, depth, WalkFilter::default(), |entry| {
                paths.push(entry.path.display().to_string());
                true
            })
            .await
            .unwrap();
        paths
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn walk_skips_entries_whose_metadata_cannot_be_read() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("locked")).unwrap();
        std::fs::write(root.join("locked/inside.rs"), "x").unwrap();
        std::fs::write(root.join("open.rs"), "x").unwrap();
        // Readable but not searchable: `read_dir` succeeds, `stat` of a member
        // does not. The listing must skip it rather than fail.
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o444))
            .unwrap();
        let searchable = std::fs::symlink_metadata(root.join("locked/inside.rs")).is_ok();
        let machine = test_machine(root.clone());
        let paths = walk_paths(&machine, &root, 2).await;
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755))
            .unwrap();

        assert!(
            paths.iter().any(|path| path.ends_with("open.rs")),
            "{paths:?}"
        );
        assert_eq!(
            paths.iter().any(|path| path.ends_with("inside.rs")),
            searchable,
            "{paths:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_swapped_in_after_the_walk_is_never_followed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), "needle").unwrap();
        std::fs::write(root.join("a.txt"), "inside").unwrap();
        let machine = test_machine(root.clone());

        // The walk sees a regular file; the read happens after the name has been
        // swapped for a symlink pointing out of the grant.
        let mut seen = 0;
        machine
            .walk_entries(&root, 1, WalkFilter::default(), |entry| {
                if entry.relative == Path::new("a.txt") {
                    seen += 1;
                }
                true
            })
            .await
            .unwrap();
        assert_eq!(seen, 1);
        std::fs::remove_file(root.join("a.txt")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), root.join("a.txt")).unwrap();

        assert!(
            machine.open_read_file(&root.join("a.txt")).await.is_err(),
            "a symlink is refused by the descriptor-rooted reader"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("secret")).unwrap(),
            "needle"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_ignore_file_is_reported_as_incomplete_not_absent() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let ignore = root.join(".gitignore");
        std::fs::write(&ignore, "match.txt\n").unwrap();
        let machine = test_machine(root.clone());
        let mut work = IgnoreWork::default();
        assert!(
            machine
                .load_gitignore(&root, None, &mut work)
                .await
                .unwrap()
                .is_some()
        );
        assert!(!work.incomplete);
        std::fs::set_permissions(&ignore, std::fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = std::fs::File::open(&ignore).is_err();
        let mut work = IgnoreWork::default();
        let loaded = machine
            .load_gitignore(&root, None, &mut work)
            .await
            .unwrap();
        std::fs::set_permissions(&ignore, std::fs::Permissions::from_mode(0o600)).unwrap();
        if unreadable {
            assert!(loaded.is_none());
            assert!(work.incomplete);
        }
        std::fs::remove_file(&ignore).unwrap();
        let mut work = IgnoreWork::default();
        assert!(
            machine
                .load_gitignore(&root, None, &mut work)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!work.incomplete);
    }

    #[tokio::test]
    async fn failed_ignore_read_charges_attempt_and_partial_bytes() {
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };
        use tokio::io::{AsyncRead, ReadBuf};

        struct PartialThenError(bool);

        impl AsyncRead for PartialThenError {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buffer: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                if !self.0 {
                    self.0 = true;
                    buffer.put_slice(b"abc");
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Ready(Err(std::io::Error::other("injected read failure")))
                }
            }
        }

        let mut work = IgnoreWork::default();
        let bytes = LocalMachine::read_ignore_bytes(PartialThenError(false), 4, &mut work)
            .await
            .unwrap();
        assert!(bytes.is_none());
        assert_eq!(work.files, 1);
        assert_eq!(work.bytes, 3);
        assert!(work.incomplete);

        let mut capped = IgnoreWork {
            bytes: MAX_IGNORE_SCAN_BYTES - 2,
            ..IgnoreWork::default()
        };
        let error = LocalMachine::read_ignore_bytes(PartialThenError(false), 3, &mut capped)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("gitignore search work exceeded"), "{error}");
        assert_eq!(capped.files, 1);
        assert_eq!(capped.bytes, MAX_IGNORE_SCAN_BYTES - 2);

        work.files = MAX_IGNORE_SCAN_FILES;
        let error = LocalMachine::read_ignore_bytes(PartialThenError(false), 4, &mut work)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("gitignore search work exceeded"), "{error}");
    }

    #[tokio::test]
    async fn ignore_aggregate_budget_never_reads_a_probe_byte() {
        use std::{
            pin::Pin,
            sync::{
                Arc,
                atomic::{AtomicUsize, Ordering},
            },
            task::{Context, Poll},
        };
        use tokio::io::{AsyncRead, ReadBuf};

        struct CountingReader {
            bytes: &'static [u8],
            position: usize,
            read: Arc<AtomicUsize>,
        }

        impl AsyncRead for CountingReader {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buffer: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                let count = (self.bytes.len() - self.position).min(buffer.remaining());
                if count > 0 {
                    buffer.put_slice(&self.bytes[self.position..self.position + count]);
                    self.position += count;
                    self.read.fetch_add(count, Ordering::Relaxed);
                }
                Poll::Ready(Ok(()))
            }
        }

        let read = Arc::new(AtomicUsize::new(0));
        let mut exact = IgnoreWork {
            bytes: MAX_IGNORE_SCAN_BYTES - 4,
            ..IgnoreWork::default()
        };
        let bytes = LocalMachine::read_ignore_bytes(
            CountingReader {
                bytes: b"abcdextra",
                position: 0,
                read: read.clone(),
            },
            4,
            &mut exact,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(bytes, b"abcd");
        assert_eq!(read.load(Ordering::Relaxed), 4);
        assert_eq!(exact.bytes, MAX_IGNORE_SCAN_BYTES);

        read.store(0, Ordering::Relaxed);
        let mut over = IgnoreWork {
            bytes: MAX_IGNORE_SCAN_BYTES - 4,
            ..IgnoreWork::default()
        };
        let error = LocalMachine::read_ignore_bytes(
            CountingReader {
                bytes: b"abcde",
                position: 0,
                read: read.clone(),
            },
            5,
            &mut over,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("gitignore search work exceeded"), "{error}");
        assert_eq!(read.load(Ordering::Relaxed), 0);
        assert_eq!(over.bytes, MAX_IGNORE_SCAN_BYTES - 4);

        assert_eq!(verify_ignore_size(4, 4, 4), IgnoreSizeVerification::Stable);
        assert_eq!(
            verify_ignore_size(4, 5, 10),
            IgnoreSizeVerification::Changed,
            "growth below both ceilings is still uncertain"
        );
        assert_eq!(
            verify_ignore_size(4, 3, 10),
            IgnoreSizeVerification::Changed,
            "shrink is also uncertain"
        );
    }

    #[tokio::test]
    async fn cumulative_ignore_work_budget_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let ignore = root.join(".gitignore");
        std::fs::write(&ignore, "#\n").unwrap();
        let machine = test_machine(root.clone());
        let mut exact = IgnoreWork {
            bytes: MAX_IGNORE_SCAN_BYTES - 2,
            files: 0,
            incomplete: false,
        };
        assert!(
            machine
                .load_gitignore(&root, None, &mut exact)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(exact.bytes, MAX_IGNORE_SCAN_BYTES);

        std::fs::write(&ignore, "match.txt\n").unwrap();
        let mut work = IgnoreWork {
            bytes: MAX_IGNORE_SCAN_BYTES - 2,
            files: 0,
            incomplete: false,
        };
        let error = machine
            .load_gitignore(&root, None, &mut work)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("gitignore search work exceeded"), "{error}");
        let mut work = IgnoreWork {
            bytes: 0,
            files: MAX_IGNORE_SCAN_FILES,
            incomplete: false,
        };
        let error = machine
            .load_gitignore(&root, None, &mut work)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("gitignore search work exceeded"), "{error}");
    }

    #[tokio::test]
    async fn denied_ignore_file_is_not_read_or_charged() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let ignore = root.join(".gitignore");
        std::fs::write(&ignore, "match.txt\n").unwrap();
        let mut machine = test_machine(root.clone());
        machine.config.access.deny_read_roots = vec![ignore];
        let mut work = IgnoreWork::default();

        assert!(
            machine
                .load_gitignore(&root, None, &mut work)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(work.files, 0);
        assert_eq!(work.bytes, 0);
        assert!(work.incomplete);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_subtree_gitignore_does_not_fail_the_walk() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("locked")).unwrap();
        std::fs::write(root.join("locked/.gitignore"), "{unclosed\n").unwrap();
        std::fs::write(root.join("open.rs"), "x").unwrap();
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o111))
            .unwrap();
        let machine = test_machine(root.clone());
        let filter = WalkFilter {
            skip_hidden: false,
            gitignore: true,
        };
        let mut paths = Vec::new();
        let outcome = machine
            .walk_entries(&root, 2, filter, |entry| {
                paths.push(entry.path.display().to_string());
                true
            })
            .await;
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755))
            .unwrap();

        // An invalid ignore file inside a directory we cannot list is not a
        // failure; the rest of the tree still reports.
        if outcome.is_ok() {
            assert!(
                paths.iter().any(|path| path.ends_with("open.rs")),
                "{paths:?}"
            );
        }
    }

    #[tokio::test]
    async fn commit_is_a_no_op_for_identical_text_and_refuses_a_stale_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join("a.txt");
        std::fs::write(&path, "one\n").unwrap();
        let machine = test_machine(root.clone());

        let snapshot = Snapshot::read(&machine, path.clone(), "a.txt")
            .await
            .unwrap();
        let verified = snapshot.verify(&machine, "editing").await.unwrap();
        assert!(
            !verified.commit(&machine, b"one\n").await.unwrap(),
            "identical text writes nothing"
        );

        // A snapshot that no longer matches is refused, and says why.
        let stale = Snapshot {
            path: path.clone(),
            display: "a.txt".to_owned(),
            bytes: b"planned\n".to_vec(),
            limit: 65536,
        };
        let error = stale
            .verify(&machine, "editing")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "a.txt changed while editing");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\n");

        // Writing through a verified snapshot replaces the file atomically and
        // leaves no temporary behind.
        let snapshot = Snapshot::read(&machine, path.clone(), "a.txt")
            .await
            .unwrap();
        let verified = snapshot.verify(&machine, "editing").await.unwrap();
        assert!(verified.commit(&machine, b"two\n").await.unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two\n");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn verified_stage_rechecks_the_snapshot_at_commit() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join("a.txt");
        std::fs::write(&path, "old\n").unwrap();
        let machine = test_machine(root);

        let snapshot = Snapshot::read(&machine, path.clone(), "a.txt")
            .await
            .unwrap();
        let verified = snapshot.verify(&machine, "editing").await.unwrap();
        let pending = verified.stage(&machine, b"new\n").await.unwrap().unwrap();
        std::fs::write(&path, "external\n").unwrap();
        let error = machine.commit_write(pending).await.unwrap_err().to_string();
        assert!(error.contains("changed before commit"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external\n");
    }

    #[tokio::test]
    async fn mutation_locks_serialize_one_path_and_allow_independent_paths() {
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = std::sync::Arc::new(test_machine(root.clone()));
        let a = root.join("a.txt");
        let b = root.join("b.txt");

        let held = machine.lock_mutations([a.as_path()]).await.unwrap();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let same = {
            let machine = machine.clone();
            let a = a.clone();
            tokio::spawn(async move {
                let _guard = machine.lock_mutations([a.as_path()]).await.unwrap();
                let _ = tx.send(());
            })
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut rx)
                .await
                .is_err(),
            "same-path lock must wait for the held guard"
        );

        let (tx, mut independent_rx) = tokio::sync::oneshot::channel();
        let independent = {
            let machine = machine.clone();
            let b = b.clone();
            tokio::spawn(async move {
                let _guard = machine.lock_mutations([b.as_path()]).await.unwrap();
                let _ = tx.send(());
            })
        };
        assert!(
            tokio::time::timeout(Duration::from_secs(1), &mut independent_rx)
                .await
                .is_ok(),
            "a different path must not be blocked"
        );
        independent.await.unwrap();

        drop(held);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), &mut rx)
                .await
                .is_ok(),
            "the waiter must acquire after the guard is released"
        );
        same.await.unwrap();
    }

    #[tokio::test]
    async fn in_flight_staging_temporaries_are_hidden_from_the_walk() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("real.txt"), "x").unwrap();
        let staging = crate::staging::staging_name();
        std::fs::write(root.join(&staging), "half-written").unwrap();
        // A name that merely resembles a staging temporary is a normal file.
        std::fs::write(root.join(".dotlink-notes.tmp"), "x").unwrap();
        let machine = test_machine(root.clone());

        let paths = walk_paths(&machine, &root, 1).await;
        assert!(
            paths.iter().any(|path| path.ends_with("real.txt")),
            "{paths:?}"
        );
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with(".dotlink-notes.tmp")),
            "{paths:?}"
        );
        assert!(
            !paths
                .iter()
                .any(|path| path.ends_with(staging.to_str().unwrap())),
            "an in-flight staging file must not be listed: {paths:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_loop_is_skipped_instead_of_failing_the_walk() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        std::fs::write(root.join("real/f.txt"), "x").unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("loop")).unwrap();
        let machine = test_machine(root.clone());

        let paths = walk_paths(&machine, &root, 4).await;
        assert!(
            paths.iter().any(|path| path.ends_with("real/f.txt")),
            "{paths:?}"
        );
        // The symlinked directory is reported, never traversed.
        assert!(paths.iter().any(|path| path.ends_with("loop")), "{paths:?}");
        assert!(
            !paths.iter().any(|path| path.contains("loop/")),
            "{paths:?}"
        );
    }
}
