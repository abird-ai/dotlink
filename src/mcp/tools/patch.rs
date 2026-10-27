//! `patch`: Codex grammar with strict preflight and per-file atomic commits.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow, bail};
use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, schemars, tool, tool_router,
};
use serde::Deserialize;
use tokio::fs;

use super::{LocalMachine, Snapshot, Verified, dot, ok_text, tool_error};
use crate::mcp::AccessNeed;
use crate::mcp::PendingWrite;
use crate::mcp::lexical_normalize;

const MAX_PATCH_FILES: usize = 32;
const MAX_PATCH_HUNKS: usize = 64;
/// The ambiguity message lists at most this many line numbers.
const MAX_AMBIGUOUS_LINES: usize = 10;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(super) struct PatchArgs {
    /// Base directory for the patch's relative paths. Relative paths resolve
    /// from the directory where dotlink was launched.
    #[serde(default = "dot")]
    dir: String,

    /// Codex patch text: `*** Begin Patch`, `*** Update File:` / `*** Add File:`
    /// sections, then `*** End Patch`.
    patch: String,

    /// Write the files that planned cleanly and report the rest instead of
    /// failing the whole call.
    #[serde(default)]
    best_effort: bool,
}

/// Codex `*** Begin Patch` grammar, v1: `*** Update File:` and
/// `*** Add File:` only. Delete and Move are rejected explicitly rather than
/// silently ignored.
#[derive(Debug)]
enum PatchOp {
    Update { path: String, hunks: Vec<PatchHunk> },
    Add { path: String, lines: Vec<String> },
}

#[derive(Debug)]
struct PatchHunk {
    anchor: Option<String>,
    lines: Vec<PatchLine>,
    /// The Codex `*** End of File` marker: this hunk must end at the last line.
    at_end_of_file: bool,
}

#[derive(Debug)]
enum PatchLine {
    Context(String),
    Removed(String),
    Added(String),
}

/// Patch lines, split on `\n` only: a CRLF patch keeps its `\r`, and
/// `apply_patch_hunks` normalizes added lines to the target file's terminator.
fn patch_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split_inclusive('\n')
        .map(|line| line.strip_suffix('\n').unwrap_or(line))
}

fn parse_patch(text: &str) -> Result<Vec<PatchOp>> {
    let mut ops: Vec<PatchOp> = Vec::new();
    let mut saw_begin = false;
    let mut saw_end = false;
    let mut hunks = 0_usize;

    for (index, line) in patch_lines(text).enumerate() {
        let number = index + 1;
        if !saw_begin {
            if line.trim().is_empty() {
                continue;
            }
            if line.trim_end() != "*** Begin Patch" {
                bail!("patch: missing '*** Begin Patch' header");
            }
            saw_begin = true;
            continue;
        }
        if saw_end {
            if !line.trim().is_empty() {
                bail!("patch: line {number}: unexpected text after '*** End Patch'");
            }
            continue;
        }

        if let Some(directive) = line.strip_prefix("*** ") {
            let directive = directive.trim_end();
            if directive == "End Patch" {
                saw_end = true;
                continue;
            }
            if directive == "End of File" {
                match ops.last_mut() {
                    Some(PatchOp::Update { hunks, .. }) => match hunks.last_mut() {
                        Some(hunk) => hunk.at_end_of_file = true,
                        None => bail!("patch: line {number}: '*** End of File' needs a hunk"),
                    },
                    _ => {
                        bail!("patch: line {number}: '*** End of File' is only valid in an update")
                    }
                }
                continue;
            }
            if let Some(path) = directive.strip_prefix("Update File:") {
                let path = path.trim().to_owned();
                if path.is_empty() {
                    bail!("patch: line {number}: '*** Update File' needs a path");
                }
                ops.push(PatchOp::Update {
                    path,
                    hunks: Vec::new(),
                });
                continue;
            }
            if let Some(path) = directive.strip_prefix("Add File:") {
                let path = path.trim().to_owned();
                if path.is_empty() {
                    bail!("patch: line {number}: '*** Add File' needs a path");
                }
                ops.push(PatchOp::Add {
                    path,
                    lines: Vec::new(),
                });
                continue;
            }
            let name = directive.split(':').next().unwrap_or(directive);
            bail!("patch: line {number}: unsupported directive '*** {name}'");
        }

        let Some(current) = ops.last_mut() else {
            bail!("patch: line {number}: text outside a file section");
        };

        if let Some(rest) = line.strip_prefix("@@") {
            let PatchOp::Update { hunks: list, .. } = current else {
                bail!("patch: line {number}: '*** Add File' does not take hunks");
            };
            if list.last().is_some_and(|hunk| hunk.at_end_of_file) {
                bail!(
                    "patch: line {number}: no hunk may follow '*** End of File' in the same file"
                );
            }
            hunks += 1;
            if hunks > MAX_PATCH_HUNKS {
                bail!("patch: {hunks} hunks, above the {MAX_PATCH_HUNKS} hunk limit");
            }
            list.push(PatchHunk {
                anchor: parse_patch_anchor(rest),
                lines: Vec::new(),
                at_end_of_file: false,
            });
            continue;
        }

        match current {
            PatchOp::Update { hunks: list, .. } => {
                let Some(hunk) = list.last_mut() else {
                    bail!("patch: line {number}: text outside a hunk");
                };
                match line.as_bytes().first() {
                    Some(b' ') => hunk.lines.push(PatchLine::Context(line[1..].to_owned())),
                    Some(b'-') => hunk.lines.push(PatchLine::Removed(line[1..].to_owned())),
                    Some(b'+') => hunk.lines.push(PatchLine::Added(line[1..].to_owned())),
                    _ if line.trim().is_empty() => {
                        hunk.lines.push(PatchLine::Context(String::new()))
                    }
                    _ => {
                        bail!("patch: line {number}: unexpected text '{line}'");
                    }
                }
            }
            PatchOp::Add { lines, .. } => match line.as_bytes().first() {
                Some(b'+') => lines.push(line[1..].to_owned()),
                _ if line.trim().is_empty() => lines.push(String::new()),
                _ => bail!("patch: line {number}: unexpected text '{line}'"),
            },
        }
    }

    if !saw_begin {
        bail!("patch: missing '*** Begin Patch' header");
    }
    if !saw_end {
        bail!("patch: missing '*** End Patch' terminator");
    }
    if ops.len() > MAX_PATCH_FILES {
        bail!(
            "patch: {} files, above the {MAX_PATCH_FILES} file limit",
            ops.len()
        );
    }
    for op in &ops {
        if let PatchOp::Update { path, hunks } = op
            && hunks.is_empty()
        {
            bail!("patch: '*** Update File' for {path} has no hunks");
        }
    }
    Ok(ops)
}

/// `@@` may carry anchor text or unified-diff line numbers. Numbers are
/// accepted and ignored; anchor text narrows where the search starts.
/// `@@` content is either anchor text or unified-diff line numbers. Both may
/// appear (`@@ -5,1 +5,1 @@ fn b()`), so leading `-`/`+` number tokens are
/// dropped and whatever follows them narrows the search.
fn parse_patch_anchor(rest: &str) -> Option<String> {
    let mut rest = rest.trim();
    loop {
        // `@@ -3,1 +3,1 @@ fn b()`: drop the range tokens, then the closing `@@`.
        let stripped = rest
            .strip_prefix('-')
            .or_else(|| rest.strip_prefix('+'))
            .map(str::trim_start)
            .filter(|stripped| stripped.starts_with(|c: char| c.is_ascii_digit()))
            .map(|stripped| {
                stripped
                    .trim_start_matches(|c: char| c.is_ascii_digit() || c == ',')
                    .trim_start()
            })
            .or_else(|| rest.strip_prefix("@@").map(str::trim_start));
        match stripped {
            Some(after) if after.len() < rest.len() => rest = after,
            _ => break,
        }
    }
    match rest.is_empty() {
        true => None,
        false => Some(rest.to_owned()),
    }
}

/// Hunk location is content-based: exact match first, then a per-line
/// `trim_end` comparison. Ambiguity is an error, and hunks within a file apply
/// top-down without re-scanning above the previous match.
fn apply_patch_hunks(
    text: &str,
    hunks: &[PatchHunk],
    display: &str,
    first_hunk: usize,
    total_hunks: usize,
) -> Result<(String, usize, usize)> {
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    let mut from_line = 0_usize;
    let mut added = 0_usize;
    let mut removed = 0_usize;

    for (offset, hunk) in hunks.iter().enumerate() {
        let position = first_hunk + offset + 1;
        let block = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                PatchLine::Context(text) => Some(text.as_str()),
                PatchLine::Removed(text) => Some(text.as_str()),
                PatchLine::Added(_) => None,
            })
            .collect::<Vec<_>>();
        let mut search_start = from_line;
        if let Some(anchor) = hunk.anchor.as_deref()
            && let Some(at) = lines
                .iter()
                .enumerate()
                .skip(search_start)
                .find(|(_, line)| line.contains(anchor))
                .map(|(index, _)| index)
        {
            search_start = at;
        }
        let near = search_start + 1;

        if block.is_empty() {
            bail!("hunk {position}/{total_hunks} ({display}): context not found near line {near}");
        }

        let mut exact = Vec::new();
        let mut lenient = Vec::new();
        for start in search_start..lines.len() {
            if start + block.len() > lines.len() {
                break;
            }
            if lines[start..start + block.len()]
                .iter()
                .zip(&block)
                .all(|(file, patch)| file == patch)
            {
                exact.push(start);
                continue;
            }
            if lines[start..start + block.len()]
                .iter()
                .zip(&block)
                .all(|(file, patch)| file.trim_end() == patch.trim_end())
            {
                lenient.push(start);
            }
        }
        let matches = if exact.is_empty() { lenient } else { exact };
        let Some(&start) = matches.first() else {
            bail!("hunk {position}/{total_hunks} ({display}): context not found near line {near}");
        };
        if matches.len() > 1 {
            let lines_listed = matches
                .iter()
                .take(MAX_AMBIGUOUS_LINES)
                .map(|line| (line + 1).to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "hunk {position}/{total_hunks} ({display}): context matches {} locations (lines {lines_listed})",
                matches.len()
            );
        }

        if hunk.at_end_of_file {
            // `text.split('\n')` yields one empty tail for a trailing newline;
            // it is not a line the marker can refer to.
            let file_lines = lines.len() - usize::from(lines.last().is_some_and(String::is_empty));
            if start + block.len() != file_lines {
                bail!(
                    "hunk {position}/{total_hunks} ({display}): '*** End of File' requires the hunk to end at the end of the file (line {file_lines})"
                );
            }
        }

        let mut replacement = Vec::with_capacity(hunk.lines.len());
        // Added lines inherit the terminator of the file they land in. The
        // patch itself often carries LF because numbered `read` strips `\r`,
        // so preserving only the patch's bytes would produce mixed endings in
        // a CRLF file.
        let file_crlf = lines.get(start).is_some_and(|line| line.ends_with('\r'));
        // Cursor into the consumed block: only context/removed lines consume
        // existing file lines.
        let mut consumed_at = 0_usize;
        for line in &hunk.lines {
            match line {
                // Keep the file's own bytes for context lines, so a lenient
                // match never rewrites trailing whitespace it did not touch.
                PatchLine::Context(_) => {
                    replacement.push(lines[start + consumed_at].clone());
                    consumed_at += 1;
                }
                PatchLine::Removed(_) => {
                    removed += 1;
                    consumed_at += 1;
                }
                PatchLine::Added(text) => {
                    added += 1;
                    let mut line = text.clone();
                    match file_crlf {
                        true if !line.ends_with('\r') => line.push('\r'),
                        false if line.ends_with('\r') => {
                            line.pop();
                        }
                        _ => {}
                    }
                    replacement.push(line);
                }
            }
        }
        let replacement_len = replacement.len();
        lines.splice(start..start + block.len(), replacement);
        from_line = start + replacement_len;
    }

    Ok((lines.join("\n"), added, removed))
}

/// Patch paths are relative to the patch's `dir`, not to the launch directory.
fn join_patch_path(dir: &str, path: &str) -> String {
    if Path::new(path).is_absolute() || dir == "." {
        return path.to_owned();
    }
    Path::new(dir).join(path).to_string_lossy().into_owned()
}

/// How a planned file is written. Update and create are different operations
/// with different rules; keeping them distinct beats overloading an `Option`.
enum PlanKind {
    Update(Snapshot),
    Create,
}

struct PlannedFile {
    path: PathBuf,
    display: String,
    kind: PlanKind,
    text: String,
    hunks: usize,
    added: usize,
    removed: usize,
}

/// A planned file that passed verification (updates) or is a fresh create.
struct ReadyFile {
    path: PathBuf,
    display: String,
    verified: Option<Verified>,
    text: String,
    hunks: usize,
    added: usize,
    removed: usize,
}

impl LocalMachine {
    async fn patch_impl(&self, args: PatchArgs) -> Result<String> {
        super::require_text_bytes(args.patch.as_bytes(), "bwrite/bedit")?;
        if args.patch.len() > self.config.max_write_bytes {
            anyhow::bail!(
                "patch: {} bytes, above the {} byte write limit",
                args.patch.len(),
                self.config.max_write_bytes
            );
        }
        let ops = parse_patch(&args.patch)?;
        let total_hunks = ops
            .iter()
            .map(|op| match op {
                PatchOp::Update { hunks, .. } => hunks.len(),
                PatchOp::Add { .. } => 0,
            })
            .sum::<usize>();

        let mut planned: Vec<PlannedFile> = Vec::new();
        let mut failures: Vec<(String, String)> = Vec::new();
        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
        let mut hunk_index = 0_usize;
        let dir = args.dir.as_str();
        // A section claims its path before it is resolved, so a section that
        // fails to resolve still prevents a later section from writing the same
        // file: the duplicate rule must not depend on resolution order.
        let claim = |path: &str| {
            let joined = join_patch_path(dir, path);
            let target = Path::new(&joined);
            match target.is_absolute() {
                true => lexical_normalize(target),
                false => lexical_normalize(&self.config.access.base_dir.join(target)),
            }
        };

        struct ResolvedSection<'a> {
            op: &'a PatchOp,
            path: PathBuf,
            display: String,
            first_hunk: usize,
        }
        let mut resolved_sections: Vec<ResolvedSection<'_>> = Vec::with_capacity(ops.len());

        for op in &ops {
            // Both sections resolve the same way, and both must reject a path
            // that another section already claimed.
            let (path, need, first_hunk) = match op {
                PatchOp::Update { path, hunks } => {
                    let first = hunk_index;
                    hunk_index += hunks.len();
                    (path, AccessNeed::ReadWrite, first)
                }
                PatchOp::Add { path, .. } => (path, AccessNeed::Write, 0),
            };
            let joined = join_patch_path(dir, path);
            let claimed = claim(path);
            let display = self.config.access.relative_display(&claimed);
            if !seen.insert(claimed.clone()) {
                let error = format!("patch: {display} appears in more than one section");
                if !args.best_effort {
                    anyhow::bail!("{error}");
                }
                failures.push((display, error));
                continue;
            }
            let resolved = match need {
                AccessNeed::Write => self.resolve_for_create(&joined, need).await,
                _ => self.resolve_existing(&joined, need).await,
            };
            let resolved = match resolved {
                Ok(resolved) => resolved,
                Err(error) => {
                    if !args.best_effort {
                        return Err(error);
                    }
                    failures.push((display, error.to_string()));
                    continue;
                }
            };
            let display = self.config.access.relative_display(&resolved);
            // The lexical claim above catches duplicates before resolution; the
            // canonical path catches two spellings (symlink aliases, a symlinked
            // parent) that name the same file. Both are needed: a section that
            // fails to resolve must still reserve the name it asked for.
            if resolved != claimed && !seen.insert(resolved.clone()) {
                let error = format!("patch: {display} appears in more than one section");
                if !args.best_effort {
                    anyhow::bail!("{error}");
                }
                failures.push((display, error));
                continue;
            }
            resolved_sections.push(ResolvedSection {
                op,
                path: resolved,
                display,
                first_hunk,
            });
        }

        // Hold every resolved target for the whole plan -> verify -> stage ->
        // commit sequence. Shard indices are acquired in sorted order, so a
        // multi-file patch cannot deadlock and independent files stay
        // concurrent.
        let _mutation = self
            .lock_mutations(
                resolved_sections
                    .iter()
                    .map(|section| section.path.as_path()),
            )
            .await?;

        for ResolvedSection {
            op,
            path: resolved,
            display,
            first_hunk,
        } in resolved_sections
        {
            match op {
                PatchOp::Update { hunks, .. } => {
                    let outcome = async {
                        let snapshot =
                            Snapshot::read(self, resolved.clone(), display.clone()).await?;
                        let text = snapshot.text()?.to_owned();
                        let (edited, added, removed) =
                            apply_patch_hunks(&text, hunks, &display, first_hunk, total_hunks)?;
                        if edited.len() > self.config.max_write_bytes {
                            anyhow::bail!(
                                "{display}: patched file would be {} bytes, above the {} byte write limit",
                                edited.len(),
                                self.config.max_write_bytes
                            );
                        }
                        Ok(PlannedFile {
                            path: resolved,
                            display: display.clone(),
                            kind: PlanKind::Update(snapshot),
                            text: edited,
                            hunks: hunks.len(),
                            added,
                            removed,
                        })
                    }
                    .await;
                    match outcome {
                        Ok(file) => planned.push(file),
                        Err(error) => {
                            if !args.best_effort {
                                return Err(error);
                            }
                            failures.push((display, error.to_string()));
                        }
                    }
                }
                PatchOp::Add { lines, .. } => {
                    // Create-only twice over: refuse an existing target while
                    // planning, and install exclusively when writing, so a file
                    // that appears in between cannot be clobbered.
                    let outcome = async {
                        match fs::symlink_metadata(&resolved).await {
                            Ok(_) => anyhow::bail!(
                                "{display}: '*** Add File' target already exists"
                            ),
                            Err(error)
                                if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error.into()),
                        }
                        let text = match lines.is_empty() {
                            true => String::new(),
                            false => format!("{}\n", lines.join("\n")),
                        };
                        if text.len() > self.config.max_write_bytes {
                            anyhow::bail!(
                                "{display}: added file would be {} bytes, above the {} byte write limit",
                                text.len(),
                                self.config.max_write_bytes
                            );
                        }
                        Ok(PlannedFile {
                            path: resolved,
                            display: display.clone(),
                            kind: PlanKind::Create,
                            text,
                            hunks: 0,
                            added: lines.len(),
                            removed: 0,
                        })
                    }
                    .await;
                    match outcome {
                        Ok(file) => planned.push(file),
                        Err(error) => {
                            if !args.best_effort {
                                return Err(error);
                            }
                            failures.push((display, error.to_string()));
                        }
                    }
                }
            }
        }

        // Verify every update before the first write. Atomic fails the whole
        // call; `best_effort` reports the file and keeps the rest.
        let mut ready: Vec<ReadyFile> = Vec::with_capacity(planned.len());
        for file in planned {
            let verified = match file.kind {
                PlanKind::Update(snapshot) => match snapshot.verify(self, "patching").await {
                    Ok(verified) => Some(verified),
                    Err(error) if args.best_effort => {
                        failures.push((file.display.clone(), format!("{error}; skipped")));
                        continue;
                    }
                    Err(error) => return Err(anyhow!("{error}; nothing was written")),
                },
                PlanKind::Create => None,
            };
            ready.push(ReadyFile {
                path: file.path,
                display: file.display,
                verified,
                text: file.text,
                hunks: file.hunks,
                added: file.added,
                removed: file.removed,
            });
        }

        // Write. Every file is staged first and only then renamed into place, so
        // a failure while writing cannot leave earlier files changed. A patch
        // that reproduces a file byte for byte touches nothing and stays out of
        // the receipt.
        struct Staged {
            file: ReadyFile,
            pending: PendingWrite,
        }
        let mut staged: Vec<Staged> = Vec::with_capacity(ready.len());
        let mut written: Vec<ReadyFile> = Vec::new();
        let mut abort: Option<anyhow::Error> = None;
        for file in ready {
            let ReadyFile {
                path,
                display,
                verified,
                text,
                hunks,
                added,
                removed,
            } = file;
            let outcome = match verified {
                Some(verified) => verified.stage(self, text.as_bytes()).await,
                // Create-only: the target is absent until commit, so a reader
                // never sees an empty file and a failure leaves nothing behind.
                None => self
                    .stage_create_write(&path, text.as_bytes())
                    .await
                    .map(Some),
            };
            let staged_file = ReadyFile {
                path,
                display,
                verified: None,
                text,
                hunks,
                added,
                removed,
            };
            match outcome {
                Ok(Some(pending)) => staged.push(Staged {
                    file: staged_file,
                    pending,
                }),
                Ok(None) => {} // byte-identical: nothing to write
                Err(error) if args.best_effort => {
                    failures.push((staged_file.display.clone(), error.to_string()))
                }
                Err(error) => {
                    abort = Some(error);
                    break;
                }
            }
        }
        if let Some(error) = abort {
            drop(staged);
            return Err(error);
        }

        for Staged { file, pending } in staged {
            match self.commit_write(pending).await {
                Ok(()) => written.push(file),
                Err(error) if args.best_effort => failures.push((file.display, error.to_string())),
                Err(error) => {
                    return Err(anyhow!(
                        "{}: commit failed after staging; earlier files in this patch may already have been replaced: {error}",
                        file.display
                    ));
                }
            }
        }

        let mode = match args.best_effort {
            true => "best_effort",
            false => "strict",
        };
        let mut receipt = serde_json::json!({
            "files": written
                .iter()
                .map(|file| serde_json::json!({
                    "path": file.display,
                    "hunks": file.hunks,
                    "added": file.added,
                    "removed": file.removed,
                }))
                .collect::<Vec<_>>(),
            "mode": mode,
        });
        if !failures.is_empty() {
            receipt["failures"] = serde_json::json!(
                failures
                    .iter()
                    .map(|(path, error)| serde_json::json!({"path": path, "error": error}))
                    .collect::<Vec<_>>()
            );
        }
        Ok(receipt.to_string())
    }
}

#[tool_router(router = "patch_router", vis = "pub(crate)")]
impl LocalMachine {
    #[tool(
        description = "Apply a Codex-grammar patch with per-file atomic replacement. Supports \"*** Update File:\" hunks and \"*** Add File:\" only; the Codex \"*** End of File\" marker is enforced (the hunk must end at the last line), while Delete and Move are rejected. At most 32 files and 64 hunks per call. Hunks are located by their context lines (exact first, then trailing-whitespace-tolerant), an ambiguous match fails, and hunks apply top-down within a file. `*** Add File` is create-only and a path claimed by two sections is refused. Every file is planned and re-checked before any commit; each file replacement is atomic, but the multi-file commit sequence is not a transaction, so a commit failure can leave earlier files replaced. best_effort writes the files that planned cleanly and reports the rest. Returns a receipt with per-file hunks/added/removed and mode (strict or best_effort).",
        annotations(
            title = "Apply patch",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn patch(&self, Parameters(args): Parameters<PatchArgs>) -> CallToolResult {
        match self.patch_impl(args).await {
            Ok(receipt) => ok_text(receipt),
            Err(error) => tool_error(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::test_support::test_machine;
    use serde_json::Value;

    fn patch_args(patch: &str) -> PatchArgs {
        PatchArgs {
            dir: dot(),
            patch: patch.to_owned(),
            best_effort: false,
        }
    }

    #[tokio::test]
    async fn patch_applies_update_and_add_sections_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("crates/x/src")).unwrap();
        std::fs::write(
            root.join("crates/x/src/lib.rs"),
            "fn main() {\n    one();\n    two();\n}\n",
        )
        .unwrap();
        let machine = test_machine(root.clone());

        let patch = r#"*** Begin Patch
*** Update File: crates/x/src/lib.rs
@@ fn main() {
 fn main() {
-    one();
+    uno();
*** Add File: crates/new/src/lib.rs
+pub fn fresh() {}
+
*** End Patch"#;
        let receipt = machine.patch_impl(patch_args(patch)).await.unwrap();
        let receipt: Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(receipt["mode"], "strict");
        assert_eq!(
            receipt["files"],
            serde_json::json!([
                {"path": "crates/x/src/lib.rs", "hunks": 1, "added": 1, "removed": 1},
                {"path": "crates/new/src/lib.rs", "hunks": 0, "added": 2, "removed": 0}
            ])
        );
        assert_eq!(
            std::fs::read_to_string(root.join("crates/x/src/lib.rs")).unwrap(),
            "fn main() {\n    uno();\n    two();\n}\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("crates/new/src/lib.rs")).unwrap(),
            "pub fn fresh() {}\n\n"
        );
    }

    #[tokio::test]
    async fn patch_parser_rejects_missing_header_terminator_and_directives() {
        let machine = test_machine(tempfile::tempdir().unwrap().path().canonicalize().unwrap());

        let error = machine
            .patch_impl(patch_args("*** Update File: a.txt\n*** End Patch"))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "patch: missing '*** Begin Patch' header");

        let error = machine
            .patch_impl(patch_args("*** Begin Patch\n*** Update File: a.txt"))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "patch: missing '*** End Patch' terminator");

        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Delete File: a.txt\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "patch: line 2: unsupported directive '*** Delete File'"
        );

        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: a.txt\n@@\n x\n*** Move to: b.txt\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "patch: line 5: unsupported directive '*** Move to'");

        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: a.txt\n@@\n x\nstray text\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "patch: line 5: unexpected text 'stray text'");
    }

    #[tokio::test]
    async fn patch_hunk_location_reports_missing_and_ambiguous_context() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "same\nsame\nsame\n").unwrap();
        std::fs::write(root.join("b.txt"), "alpha\nbeta\n").unwrap();
        let machine = test_machine(root.clone());

        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: b.txt\n@@\n-nope\n+yep\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "hunk 1/1 (b.txt): context not found near line 1");

        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: a.txt\n@@\n-same\n+other\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "hunk 1/1 (a.txt): context matches 3 locations (lines 1, 2, 3)"
        );

        // Nothing was written by either failure.
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "same\nsame\nsame\n"
        );
    }

    #[tokio::test]
    async fn patch_falls_back_to_trim_end_and_applies_hunks_top_down() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "first  \nsecond\nthird\nfourth\n").unwrap();
        let machine = test_machine(root.clone());

        machine
            .patch_impl(patch_args(
                "*** Begin Patch\n\
                 *** Update File: a.txt\n\
                 @@\n\
                 -first\n\
                 +FIRST\n\
                 @@\n\
                 -third\n\
                 +THIRD\n\
                 *** End Patch",
            ))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "FIRST\nsecond\nTHIRD\nfourth\n"
        );
    }

    #[tokio::test]
    async fn patch_add_file_refuses_existing_targets() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "present\n").unwrap();
        let machine = test_machine(root.clone());

        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Add File: a.txt\n+new\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "a.txt: '*** Add File' target already exists");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "present\n"
        );
    }

    #[tokio::test]
    async fn patch_best_effort_writes_what_planned_cleanly() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("good.txt"), "old\n").unwrap();
        std::fs::write(root.join("exists.txt"), "keep\n").unwrap();
        let machine = test_machine(root.clone());

        let patch = "*** Begin Patch\n\
                     *** Update File: good.txt\n\
                     @@\n\
                     -old\n\
                     +new\n\
                     *** Update File: missing.txt\n\
                     @@\n\
                     -x\n\
                     +y\n\
                     *** Add File: exists.txt\n\
                     +nope\n\
                     *** End Patch";
        // Atomic mode fails the whole call.
        let error = machine.patch_impl(patch_args(patch)).await.unwrap_err();
        assert!(error.to_string().contains("missing.txt"), "{error}");
        assert_eq!(
            std::fs::read_to_string(root.join("good.txt")).unwrap(),
            "old\n"
        );

        let mut args = patch_args(patch);
        args.best_effort = true;
        let receipt: Value =
            serde_json::from_str(&machine.patch_impl(args).await.unwrap()).unwrap();
        assert_eq!(receipt["mode"], "best_effort");
        assert_eq!(receipt["files"][0]["path"], "good.txt");
        assert_eq!(receipt["failures"].as_array().unwrap().len(), 2);
        assert_eq!(
            std::fs::read_to_string(root.join("good.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("exists.txt")).unwrap(),
            "keep\n"
        );
    }

    #[test]
    fn patch_parser_bounds_hunks_and_files() {
        let mut patch = String::from("*** Begin Patch\n*** Update File: a.txt\n");
        for _ in 0..=MAX_PATCH_HUNKS {
            patch.push_str("@@\n x\n");
        }
        patch.push_str("*** End Patch\n");
        let error = parse_patch(&patch).unwrap_err().to_string();
        assert!(error.contains("hunk limit"), "{error}");

        let mut patch = String::from("*** Begin Patch\n");
        for index in 0..=MAX_PATCH_FILES {
            patch.push_str(&format!("*** Add File: f{index}.txt\n+x\n"));
        }
        patch.push_str("*** End Patch\n");
        let error = parse_patch(&patch).unwrap_err().to_string();
        assert!(error.contains("file limit"), "{error}");

        assert_eq!(
            parse_patch(
                "*** Begin Patch\n*** Update File: a.txt\n@@ -12,3 +12,4 @@\n-x\n+y\n*** End Patch"
            )
            .unwrap()
            .len(),
            1
        );
        let ops = parse_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End of File\n*** End Patch",
        )
        .unwrap();
        match &ops[0] {
            PatchOp::Update { hunks, .. } => assert!(hunks[0].at_end_of_file),
            other => panic!("{other:?}"),
        }
        assert!(
            parse_patch("*** Begin Patch\n*** Add File: a.txt\n+one\n two\n*** End Patch").is_err()
        );
    }

    #[tokio::test]
    async fn patch_enforces_the_end_of_file_marker() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let machine = test_machine(root.clone());

        // The block is at the end of the file: accepted.
        let receipt: Value = serde_json::from_str(
            &machine
                .patch_impl(patch_args(
                    "*** Begin Patch\n*** Update File: a.txt\n@@\n-three\n+THREE\n*** End of File\n*** End Patch",
                ))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["files"][0]["path"], "a.txt");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\ntwo\nTHREE\n"
        );

        // The same block in the middle of the file is refused, and nothing is
        // written: the marker is a claim the patch must satisfy.
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** End of File\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("requires the hunk to end at the end of the file"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );

        // A hunk after the marker is contradictory.
        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: a.txt\n@@\n-three\n+THREE\n*** End of File\n@@\n-one\n+ONE\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no hunk may follow"), "{error}");
    }

    #[tokio::test]
    async fn patch_reports_a_snapshot_that_changed_during_planning() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("crates/y/src")).unwrap();
        std::fs::write(root.join("crates/y/src/lib.rs"), "changed\n").unwrap();
        let machine = test_machine(root.clone());
        let stale = Snapshot {
            path: root.join("crates/y/src/lib.rs"),
            display: "crates/y/src/lib.rs".to_owned(),
            bytes: b"planned".to_vec(),
            limit: 65536,
        };
        let error = stale
            .verify(&machine, "patching")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "crates/y/src/lib.rs changed while patching");
    }

    #[test]
    fn the_advertised_description_states_the_enforced_limits() {
        let tool = LocalMachine::all_tools().get("patch").cloned().unwrap();
        let description = tool.description.clone().unwrap_or_default();
        for limit in [MAX_PATCH_FILES, MAX_PATCH_HUNKS] {
            assert!(
                description.contains(&limit.to_string()),
                "the patch description must advertise its {limit} limit: {description}"
            );
        }
    }

    #[test]
    fn patch_anchor_text_is_kept_and_line_numbers_are_ignored() {
        assert_eq!(
            parse_patch_anchor(" fn main() {"),
            Some("fn main() {".to_owned())
        );
        assert_eq!(parse_patch_anchor(" -12,3 +12,4"), None);
        assert_eq!(parse_patch_anchor(" +12,4"), None);
        assert_eq!(
            parse_patch_anchor(" -3,1 +3,1 @@ fn b()"),
            Some("fn b()".to_owned())
        );
        let ops = parse_patch(
            "*** Begin Patch\n*** Update File: a.rs\n@@ -3,1 +3,1 @@ fn b()\n-let x = 1;\n+let x = 2;\n*** End Patch",
        )
        .unwrap();
        match &ops[0] {
            PatchOp::Update { hunks, .. } => {
                assert_eq!(hunks[0].anchor.as_deref(), Some("fn b()"));
                assert_eq!(hunks[0].lines.len(), 2, "removed + added");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(parse_patch_anchor(""), None);
    }

    #[tokio::test]
    async fn patch_best_effort_reports_a_create_failure_and_keeps_the_rest() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("present.txt"), "present\n").unwrap();
        let machine = test_machine(root.clone());

        let mut args = patch_args(
            "*** Begin Patch\n\
             *** Add File: present.txt\n\
             +nope\n\
             *** Add File: fresh.txt\n\
             +yes\n\
             *** End Patch",
        );
        args.best_effort = true;
        let receipt: Value =
            serde_json::from_str(&machine.patch_impl(args).await.unwrap()).unwrap();
        assert_eq!(receipt["files"][0]["path"], "fresh.txt");
        assert_eq!(receipt["failures"][0]["path"], "present.txt");
        assert_eq!(
            std::fs::read_to_string(root.join("fresh.txt")).unwrap(),
            "yes\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("present.txt")).unwrap(),
            "present\n"
        );
    }

    #[tokio::test]
    async fn patch_create_only_write_refuses_an_existing_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("present.txt"), "present\n").unwrap();
        let machine = test_machine(root.clone());

        let error = machine
            .stage_create_write(&root.join("present.txt"), b"new")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
        assert_eq!(
            std::fs::read_to_string(root.join("present.txt")).unwrap(),
            "present\n"
        );

        // A create-only stage does not create the target until commit.
        let staged = machine
            .stage_create_write(&root.join("fresh.txt"), b"fresh")
            .await
            .unwrap();
        assert!(!root.join("fresh.txt").exists());
        machine.commit_write(staged).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("fresh.txt")).unwrap(),
            "fresh"
        );
    }

    #[tokio::test]
    async fn concurrent_patches_serialize_per_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let original = (0..8)
            .map(|index| format!("token{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.join("f.txt"), format!("{original}\n")).unwrap();
        let machine = std::sync::Arc::new(test_machine(root.clone()));
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..8 {
            let machine = machine.clone();
            tasks.spawn(async move {
                let patch = format!(
                    "*** Begin Patch\n*** Update File: f.txt\n@@\n-token{index}\n+CHANGED{index}\n*** End Patch"
                );
                machine.patch_impl(patch_args(&patch)).await
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap().unwrap();
        }
        let text = std::fs::read_to_string(root.join("f.txt")).unwrap();
        for index in 0..8 {
            assert!(text.contains(&format!("CHANGED{index}")), "{text}");
        }
    }

    #[tokio::test]
    async fn patch_rewrites_nothing_when_a_hunk_reproduces_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join("a.txt");
        std::fs::write(&path, "same\n").unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let machine = test_machine(root);

        let receipt: Value = serde_json::from_str(
            &machine
                .patch_impl(patch_args(
                    "*** Begin Patch\n*** Update File: a.txt\n@@\n same\n*** End Patch",
                ))
                .await
                .unwrap(),
        )
        .unwrap();
        // A no-op patch writes nothing and stays out of the receipt.
        assert_eq!(receipt["files"], serde_json::json!([]));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "same\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "an unchanged patch must not touch the file"
        );
    }

    #[tokio::test]
    async fn patch_refuses_a_path_claimed_by_two_sections() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("existing.txt"), "old\n").unwrap();
        let machine = test_machine(root.clone());

        // Two Add sections for the same new path: refused while planning, so
        // strict mode writes nothing at all.
        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Add File: dup.txt\n+one\n*** Add File: dup.txt\n+two\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("dup.txt appears in more than one section"),
            "{error}"
        );
        assert!(!root.join("dup.txt").exists(), "nothing may be written");

        // An Update and an Add for the same path are refused too, in either
        // order.
        for patch in [
            "*** Begin Patch\n*** Update File: existing.txt\n@@\n-old\n+new\n*** Add File: existing.txt\n+x\n*** End Patch",
            "*** Begin Patch\n*** Add File: existing.txt\n+x\n*** Update File: existing.txt\n@@\n-old\n+new\n*** End Patch",
        ] {
            assert!(
                machine.patch_impl(patch_args(patch)).await.is_err(),
                "{patch}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(root.join("existing.txt")).unwrap(),
            "old\n"
        );

        // best_effort reports the conflict instead of writing the second copy.
        let receipt: Value = serde_json::from_str(
            &machine
                .patch_impl(PatchArgs {
                    dir: ".".to_owned(),
                    patch: "*** Begin Patch\n*** Add File: best.txt\n+one\n*** Add File: best.txt\n+two\n*** End Patch".to_owned(),
                    best_effort: true,
                })
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["files"].as_array().unwrap().len(), 1);
        assert!(
            receipt["failures"][0]["error"]
                .as_str()
                .unwrap()
                .contains("appears in more than one section")
        );
        assert_eq!(
            std::fs::read_to_string(root.join("best.txt")).unwrap(),
            "one\n"
        );
    }

    #[tokio::test]
    async fn patch_add_treats_only_a_missing_target_as_absent() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        // A directory where the file should go: `mkdir` of a file target is an
        // error, not "absent", so planning must fail.
        std::fs::create_dir(root.join("taken")).unwrap();
        let machine = test_machine(root.clone());
        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Add File: taken\n+x\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("already exists"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn patch_write_failure_leaves_every_file_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("good.txt"), "old\n").unwrap();
        std::fs::create_dir(root.join("locked")).unwrap();
        std::fs::set_permissions(
            root.join("locked"),
            std::os::unix::fs::PermissionsExt::from_mode(0o555),
        )
        .unwrap();
        let machine = test_machine(root.clone());

        // The Add cannot be written, so the Update staged before it must not be
        // visible either: staging happens for every file before any rename.
        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: good.txt\n@@\n-old\n+new\n*** Add File: locked/new.txt\n+x\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        std::fs::set_permissions(
            root.join("locked"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        assert!(error.contains("locked/new.txt"), "{error}");
        assert_eq!(
            std::fs::read_to_string(root.join("good.txt")).unwrap(),
            "old\n",
            "a failed write leaves earlier files untouched"
        );
        assert!(!root.join("locked/new.txt").exists());
        // No staged temporary survives the failure.
        assert!(
            std::fs::read_dir(&root)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .all(|entry| !entry.file_name().to_string_lossy().contains(".dotlink-"))
        );
    }

    #[tokio::test]
    async fn patch_inherits_crlf_for_added_lines() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("crlf.txt"), "foo\r\nbar\r\n").unwrap();
        let machine = test_machine(root.clone());

        // A model reading numbered output sees LF; the patch has no CR. The
        // added line must still use the file's CRLF terminator.
        machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: crlf.txt\n@@\n-bar\n+baz\n*** End Patch",
            ))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(root.join("crlf.txt")).unwrap(),
            b"foo\r\nbaz\r\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn patch_refuses_two_spellings_of_one_symlinked_file() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("real.txt"), "old\n").unwrap();
        symlink("real.txt", root.join("alias.txt")).unwrap();
        let machine = test_machine(root.clone());

        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: real.txt\n@@\n-old\n+one\n*** Update File: alias.txt\n@@\n-old\n+two\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("appears in more than one section"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("real.txt")).unwrap(),
            "old\n"
        );

        let mut args = patch_args(
            "*** Begin Patch\n*** Update File: real.txt\n@@\n-old\n+one\n*** Update File: alias.txt\n@@\n-old\n+two\n*** End Patch",
        );
        args.best_effort = true;
        let receipt: Value =
            serde_json::from_str(&machine.patch_impl(args).await.unwrap()).unwrap();
        assert_eq!(receipt["files"][0]["path"], "real.txt");
        assert!(
            receipt["failures"][0]["error"]
                .as_str()
                .unwrap()
                .contains("appears in more than one section")
        );
        assert_eq!(
            std::fs::read_to_string(root.join("real.txt")).unwrap(),
            "one\n"
        );
    }

    #[tokio::test]
    async fn patch_claims_a_path_before_it_resolves_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = test_machine(root.clone());

        // The first section names a missing file, so it fails to resolve. The
        // Add still claims the same path, so `best_effort` must report the
        // duplicate instead of writing the file the first section named.
        let receipt: Value = serde_json::from_str(
            &machine
                .patch_impl(PatchArgs {
                    dir: ".".to_owned(),
                    patch: "*** Begin Patch\n*** Update File: new.txt\n@@\n-x\n+y\n*** Add File: new.txt\n+z\n*** End Patch".to_owned(),
                    best_effort: true,
                })
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["files"], serde_json::json!([]));
        assert_eq!(receipt["failures"].as_array().unwrap().len(), 2);
        assert!(
            receipt["failures"][1]["error"]
                .as_str()
                .unwrap()
                .contains("appears in more than one section"),
            "{receipt}"
        );
        assert!(!root.join("new.txt").exists());
    }

    #[tokio::test]
    async fn patch_anchor_accepts_numbers_and_text_together() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.rs"), "let x = 1;\nfn b() {}\nlet x = 1;\n").unwrap();
        let machine = test_machine(root.clone());

        // Without an anchor the block is ambiguous.
        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: a.rs\n@@\n-let x = 1;\n+let x = 2;\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "hunk 1/1 (a.rs): context matches 2 locations (lines 1, 3)"
        );

        // An anchor carrying both range numbers and text: the numbers are
        // ignored and `fn b()` moves the search past the first `let x`.
        machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: a.rs\n@@ -3,1 +3,1 @@ fn b()\n-let x = 1;\n+let x = 2;\n*** End Patch",
            ))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "let x = 1;\nfn b() {}\nlet x = 2;\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn patch_undoes_a_claimed_add_when_a_later_file_fails() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("locked")).unwrap();
        std::fs::write(root.join("locked/ro.txt"), "old\n").unwrap();
        std::fs::set_permissions(
            root.join("locked"),
            std::os::unix::fs::PermissionsExt::from_mode(0o555),
        )
        .unwrap();
        let machine = test_machine(root.clone());

        // The Add succeeds first; the Update then cannot be staged. Atomic mode
        // must leave neither the new file nor a changed target behind.
        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Add File: claimed.txt\n+x\n*** Update File: locked/ro.txt\n@@\n-old\n+new\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        std::fs::set_permissions(
            root.join("locked"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        assert!(error.contains("locked/ro.txt"), "{error}");
        assert!(!root.join("claimed.txt").exists(), "the claim is undone");
        assert_eq!(
            std::fs::read_to_string(root.join("locked/ro.txt")).unwrap(),
            "old\n"
        );
        assert!(
            std::fs::read_dir(&root)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .all(|entry| !entry.file_name().to_string_lossy().contains(".dotlink-"))
        );
    }
}
