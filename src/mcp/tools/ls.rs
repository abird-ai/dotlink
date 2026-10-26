//! `ls`: descriptor-rooted discovery with one glob grammar.

use std::{cmp::Ordering, collections::BinaryHeap};

use anyhow::Result;
use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, schemars, tool, tool_router,
};
use serde::{Deserialize, Serialize};
use tokio::fs;

use super::{
    GlobFilter, LocalMachine, WalkFilter, dot, ok_text, retain_best, tool_error, trim_to_ceiling,
};
use crate::mcp::AccessNeed;

const DEFAULT_MAX_LIST_ENTRIES: usize = 4 * 1024;
const MAX_LIST_ENTRIES: usize = 16 * 1024;

#[cfg(test)]
impl Default for ListArgs {
    fn default() -> Self {
        Self {
            path: dot(),
            pattern: None,
            depth: None,
            kind: ListKind::Any,
            sort: ListSort::Path,
            limit: None,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ListArgs {
    /// Directory path. Relative paths resolve from the directory where dotlink was launched.
    #[serde(default = "dot")]
    pub(super) path: String,

    /// Glob over entry paths relative to `path`, and over the bare entry name.
    /// `*`, `?`, `[...]`, `{a,b}` stay within one path segment; `**` recurses.
    #[serde(default)]
    pub(super) pattern: Option<String>,

    /// Directory levels to descend: 1, or unbounded when `pattern` contains
    /// `**` (4 for a pattern with `/`). An explicit value always wins.
    /// Symlinked directories are never traversed.
    #[serde(default)]
    pub(super) depth: Option<usize>,

    /// Entry kind filter.
    #[serde(default)]
    pub(super) kind: ListKind,

    /// Entry ordering. `mtime` is newest first; undated entries sort last.
    #[serde(default)]
    pub(super) sort: ListSort,

    /// Maximum number of entries to return. Defaults to 4096 and is capped at 16384.
    #[serde(default)]
    pub(super) limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ListKind {
    #[default]
    Any,
    File,
    Dir,
}

impl ListKind {
    fn accepts(self, kind: &str) -> bool {
        match self {
            Self::Any => true,
            Self::File => kind == "file",
            Self::Dir => kind == "dir",
        }
    }
}

#[derive(
    Debug, Clone, Copy, Default, Eq, PartialEq, Ord, PartialOrd, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ListSort {
    #[default]
    Path,
    Mtime,
}

#[derive(Debug, Serialize)]
struct FsEntry {
    path: String,
    kind: &'static str,
    size: u64,
    /// Ordering input only; never serialized.
    #[serde(skip)]
    mtime: Option<std::time::SystemTime>,
}

/// Heap ordering: the worst retained result compares greatest.
struct RankedEntry {
    entry: FsEntry,
    sort: ListSort,
}

impl PartialEq for RankedEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for RankedEntry {}
impl PartialOrd for RankedEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for RankedEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sort.cmp(&other.sort).then_with(|| match self.sort {
            ListSort::Path => self.entry.path.cmp(&other.entry.path),
            ListSort::Mtime => other
                .entry
                .mtime
                .cmp(&self.entry.mtime)
                .then_with(|| self.entry.path.cmp(&other.entry.path)),
        })
    }
}

fn list_entry_limit(requested: Option<usize>) -> usize {
    requested
        .unwrap_or(DEFAULT_MAX_LIST_ENTRIES)
        .clamp(1, MAX_LIST_ENTRIES)
}

/// Explicit `depth` always wins. Otherwise the pattern decides: `**` recurses
/// without a level cap (the same rule `grep` applies), and any other pattern
/// needs at most as many levels as it has path segments, because `*` and `?`
/// never cross `/`. Counting segments is exact enough that a deep pattern
/// cannot silently return nothing. Entry collection is still bounded by
/// `MAX_LIST_ENTRIES`.
fn list_depth(requested: Option<usize>, pattern: Option<&str>) -> usize {
    if let Some(depth) = requested {
        return depth.max(1);
    }
    match pattern {
        Some(pattern) if pattern.contains("**") => usize::MAX,
        Some(pattern) => pattern.split('/').count().max(1),
        None => 1,
    }
}

impl LocalMachine {
    async fn list_impl(&self, args: ListArgs) -> Result<(Vec<FsEntry>, bool)> {
        let root = self.resolve_existing(&args.path, AccessNeed::Read).await?;
        if !fs::metadata(&root).await?.is_dir() {
            anyhow::bail!("not a directory: {}", args.path);
        }

        let depth = list_depth(args.depth, args.pattern.as_deref());
        let pattern = GlobFilter::optional(args.pattern.as_deref())?;
        let limit = list_entry_limit(args.limit);
        let mut selected = BinaryHeap::with_capacity(limit);
        let mut truncated = false;

        // Scanning completes before sorting, so any cap selects the actual
        // global path/newest prefix rather than the first entries enumerated.
        self.walk_entries(&root, depth, WalkFilter::default(), |entry| {
            if pattern
                .as_ref()
                .is_some_and(|pattern| !pattern.matches_entry(&entry.relative))
            {
                return true;
            }
            if !args.kind.accepts(entry.kind) {
                return true;
            }
            truncated |= retain_best(
                &mut selected,
                RankedEntry {
                    entry: FsEntry {
                        path: self.config.access.relative_display(&entry.path),
                        kind: entry.kind,
                        size: entry.size,
                        mtime: entry.mtime,
                    },
                    sort: args.sort,
                },
                limit,
            );
            true
        })
        .await?;

        let mut output: Vec<FsEntry> = selected.into_iter().map(|item| item.entry).collect();
        match args.sort {
            ListSort::Path => output.sort_by(|a, b| a.path.cmp(&b.path)),
            ListSort::Mtime => {
                output.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.path.cmp(&b.path)))
            }
        }
        // The profile byte ceiling bounds the reply itself, not only the entry
        // count. The renderer is the exact handler shape so the measured and
        // returned JSON cannot drift.
        let render = |entries: &[FsEntry], truncated: bool| {
            serde_json::json!({ "entries": entries, "truncated": truncated }).to_string()
        };
        let (output, truncated) =
            trim_to_ceiling(output, self.config.max_output_bytes, truncated, render);
        Ok((output, truncated))
    }
}

#[tool_router(router = "discovery_router", vis = "pub(crate)")]
impl LocalMachine {
    #[tool(
        description = "List files and directories. `depth` defaults to 1, to the pattern's own path-segment count, or to unbounded when the pattern contains `**`; an explicit `depth` always wins. Symlinked directories are never traversed, and in-flight `.dotlink-*.tmp` staging files are hidden. Returns {\"entries\":[{\"path\",\"kind\",\"size\"}],\"truncated\":bool}; the rendered reply also respects `max_output_bytes`.",
        annotations(
            title = "List files",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(super) async fn ls(&self, Parameters(args): Parameters<ListArgs>) -> CallToolResult {
        match self.list_impl(args).await {
            Ok((entries, truncated)) => {
                let payload = serde_json::json!({ "entries": entries, "truncated": truncated });
                ok_text(payload.to_string())
            }
            Err(error) => tool_error(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::test_support::test_machine;

    #[tokio::test]
    async fn ls_globs_and_limits_depth() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join("top.rs"), "x").unwrap();
        std::fs::write(root.join("src/lib.rs"), "x").unwrap();
        std::fs::write(root.join("src/nested/deep.rs"), "x").unwrap();
        std::fs::write(root.join("README.md"), "x").unwrap();
        let machine = test_machine(root);

        // Default depth 1: this directory only.
        let (entries, truncated) = machine
            .list_impl(ListArgs {
                path: ".".to_owned(),
                pattern: None,
                depth: None,
                ..ListArgs::default()
            })
            .await
            .unwrap();
        assert!(!truncated);
        let names: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(names, ["README.md", "src", "top.rs"]);

        // Depth 2 descends one more level.
        let (entries, _) = machine
            .list_impl(ListArgs {
                path: ".".to_owned(),
                pattern: None,
                depth: Some(2),
                ..ListArgs::default()
            })
            .await
            .unwrap();
        let names: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert!(names.contains(&"src/lib.rs"), "{names:?}");
        assert!(!names.contains(&"src/nested/deep.rs"), "{names:?}");

        // Pattern filters every level it descends into.
        let (entries, _) = machine
            .list_impl(ListArgs {
                path: ".".to_owned(),
                pattern: Some("*.rs".to_owned()),
                depth: Some(3),
                ..ListArgs::default()
            })
            .await
            .unwrap();
        let mut names: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["src/lib.rs", "src/nested/deep.rs", "top.rs"]);

        // A capped listing reports truncation instead of silently dropping entries.
        let (entries, truncated) = machine
            .list_impl(ListArgs {
                path: ".".to_owned(),
                pattern: Some("*.rs".to_owned()),
                depth: Some(3),
                limit: Some(2),
                ..ListArgs::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert!(truncated);
    }

    #[tokio::test]
    async fn ls_limit_preserves_newest_file_even_if_enumerated_later() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("old.txt"), "older").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(25));
        std::fs::write(root.join("new.txt"), "newer").unwrap();
        let machine = test_machine(root);
        let (entries, truncated) = machine
            .list_impl(ListArgs {
                path: ".".into(),
                sort: ListSort::Mtime,
                limit: Some(1),
                ..ListArgs::default()
            })
            .await
            .unwrap();
        assert!(truncated);
        assert_eq!(entries[0].path, "new.txt");
    }

    #[tokio::test]
    async fn ls_depth_follows_patterns_kind_and_mtime() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "x").unwrap();
        std::fs::write(root.join("src/nested/deep.rs"), "x").unwrap();
        std::fs::write(root.join("top.rs"), "x").unwrap();
        let machine = test_machine(root.clone());

        // A pattern addressing a nested path implies the depth it spans.
        let (entries, _) = machine
            .list_impl(ListArgs {
                path: ".".to_owned(),
                pattern: Some("src/**/*.rs".to_owned()),
                ..ListArgs::default()
            })
            .await
            .unwrap();
        let mut names: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["src/lib.rs", "src/nested/deep.rs"]);

        // An explicit depth still wins over the implied default.
        let (entries, _) = machine
            .list_impl(ListArgs {
                path: ".".to_owned(),
                pattern: Some("src/**/*.rs".to_owned()),
                depth: Some(1),
                ..ListArgs::default()
            })
            .await
            .unwrap();
        assert!(entries.is_empty(), "{entries:?}");

        // `kind` filters entries without stopping the descent.
        let (entries, _) = machine
            .list_impl(ListArgs {
                path: ".".to_owned(),
                kind: ListKind::File,
                depth: Some(3),
                ..ListArgs::default()
            })
            .await
            .unwrap();
        assert!(
            entries.iter().all(|entry| entry.kind == "file"),
            "{entries:?}"
        );
        assert_eq!(entries.len(), 3, "{entries:?}");

        // `sort=mtime` returns newest first; `path` is alphabetical.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(root.join("newest.rs"), "x").unwrap();
        let (entries, _) = machine
            .list_impl(ListArgs {
                path: ".".to_owned(),
                sort: ListSort::Mtime,
                ..ListArgs::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.first().unwrap().path, "newest.rs", "{entries:?}");
        let (entries, _) = machine
            .list_impl(ListArgs {
                path: ".".to_owned(),
                sort: ListSort::Path,
                ..ListArgs::default()
            })
            .await
            .unwrap();
        let names: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(names, ["newest.rs", "src", "top.rs"]);
    }

    #[test]
    fn depth_follows_the_pattern_instead_of_a_fixed_default() {
        assert_eq!(list_depth(None, None), 1);
        assert_eq!(list_depth(None, Some("*.rs")), 1);
        // `**` walks without a level cap; the entry count still bounds it.
        assert_eq!(list_depth(None, Some("**/*.rs")), usize::MAX);
        // Any other pattern needs at most one level per path segment.
        assert_eq!(list_depth(None, Some("src/lib.rs")), 2);
        assert_eq!(list_depth(None, Some("a/b/c/d/e/*.rs")), 6);
        assert_eq!(list_depth(None, Some("src/")), 2);
        assert_eq!(list_depth(Some(2), Some("src/**")), 2);
        assert_eq!(list_depth(Some(0), None), 1);
    }

    #[tokio::test]
    async fn a_deep_pattern_still_finds_its_entries() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("a/b/c/d/e")).unwrap();
        std::fs::write(root.join("a/b/c/d/e/leaf.rs"), "x").unwrap();
        let machine = test_machine(root);

        let (entries, truncated) = machine
            .list_impl(ListArgs {
                path: ".".to_owned(),
                pattern: Some("a/b/c/d/e/*.rs".to_owned()),
                ..ListArgs::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(!truncated);
    }

    #[tokio::test]
    async fn ls_reply_is_bounded_by_output_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for index in 0..12 {
            std::fs::write(root.join(format!("{index:02}-{}", "x".repeat(80))), "x").unwrap();
        }
        let machine = crate::mcp::tools::test_support::test_machine_with(root, |config| {
            config.max_output_bytes = 1024
        });
        let (entries, truncated) = machine
            .list_impl(ListArgs {
                path: ".".to_owned(),
                ..ListArgs::default()
            })
            .await
            .unwrap();
        let rendered = serde_json::json!({"entries": entries, "truncated": truncated}).to_string();
        assert!(truncated, "{rendered}");
        assert!(rendered.len() <= 1024, "{} bytes", rendered.len());
    }

    #[test]
    fn list_entry_limit_defaults_to_4096_and_caps_at_16384() {
        assert_eq!(list_entry_limit(None), 4 * 1024);
        assert_eq!(list_entry_limit(Some(10)), 10);
        assert_eq!(list_entry_limit(Some(1_000_000)), 16 * 1024);
        assert_eq!(list_entry_limit(Some(0)), 1);
    }
}
