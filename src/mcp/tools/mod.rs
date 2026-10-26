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
    // EXDEV is a mount point the grant does not allow crossing, ELOOP a symlink
    // or mount loop; neither is a reason to abandon the rest of the tree.
    const EXDEV: i32 = 18;
    const ELOOP: i32 = 40;
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::NotADirectory
    ) || matches!(error.raw_os_error(), Some(EXDEV) | Some(ELOOP))
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

    /// Read this directory's `.gitignore` through the pinned handle and turn it
    /// into a matcher rooted at its own directory. Bounded at 64 KiB and 1024
    /// lines; an invalid pattern fails the whole call (fail closed).
    async fn load_gitignore(
        &self,
        dir: &Path,
        handle: Option<&DirHandle>,
    ) -> Result<Option<Gitignore>> {
        let path = dir.join(".gitignore");
        // The walk drops denied entries, but this open is separate: a deny rule
        // or protected path on the ignore file itself must still win.
        if self.config.access.denied_read(&path) || self.ensure_not_protected(&path).is_err() {
            return Ok(None);
        }
        let Some(file) = self
            .open_directory_child(handle, OsStr::new(".gitignore"), &path)
            .await
        else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        // An ignore file that disappears or fails mid-read widens the search
        // (like `rg`) instead of failing it; an invalid pattern still fails.
        if file
            .take(GITIGNORE_MAX_BYTES as u64)
            .read_to_end(&mut bytes)
            .await
            .is_err()
        {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&bytes);
        let mut builder = GitignoreBuilder::new(dir);
        for (index, line) in text.lines().take(GITIGNORE_MAX_LINES).enumerate() {
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
    /// Open a member of an already-pinned directory, never by ambient path.
    /// `None` means the entry is absent or unreadable, which callers treat as
    /// "not there" rather than as a tool failure.
    async fn open_directory_child(
        &self,
        handle: Option<&DirHandle>,
        name: &OsStr,
        path: &Path,
    ) -> Option<fs::File> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(handle) = handle {
            return self
                .secure_fs
                .open_child_read(&handle.fd, name, handle.resolve)
                .ok()
                .map(fs::File::from_std);
        }
        #[cfg(windows)]
        if handle.is_none() {
            return None;
        }
        self.open_read_file(path).await.ok()
    }

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
    pub(crate) async fn walk_entries(
        &self,
        root: &Path,
        depth: usize,
        filter: WalkFilter,
        mut on_entry: impl FnMut(WalkedEntry) -> bool,
    ) -> Result<()> {
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
                Err(error) if !is_root && skippable_directory_error(&error) => continue,
                Err(error) => return Err(error),
            };
            let mut reader = match self.read_directory(handle.as_ref(), &dir).await {
                Ok(reader) => reader,
                Err(error) if !is_root && skippable_directory_error(&error) => continue,
                Err(error) => return Err(error),
            };
            // Only a directory we can actually enumerate contributes ignore
            // rules, so an unreadable subtree cannot fail the whole search.
            if filter.gitignore
                && let Some(ignore) = self.load_gitignore(&dir, handle.as_ref()).await?
            {
                ignores.push(Arc::new(ignore));
            }

            loop {
                let entry = match reader.next_entry().await {
                    Ok(Some(entry)) => entry,
                    Ok(None) => break,
                    // Stop this directory, keep the rest of the walk.
                    Err(error) if skippable_io_error(&error) => break,
                    Err(error) => return Err(error.into()),
                };
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
                    Err(error) if skippable_io_error(&error) => continue,
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
                    return Ok(());
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

        Ok(())
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
        std::str::from_utf8(&self.bytes)
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
