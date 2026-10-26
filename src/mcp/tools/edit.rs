//! `edit`: ordered multi-file exact-text replacement with per-file atomic commits.

use anyhow::Result;
use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, schemars, tool, tool_router,
};
use serde::Deserialize;

use super::{
    LocalMachine, Snapshot, has_line_number_prefix, line_count, line_number, match_indices,
    ok_text, tool_error,
};
use crate::mcp::AccessNeed;

const MAX_EDIT_OPS: usize = 64;
const MAX_EDIT_FILES: usize = 16;
/// The ambiguity message lists at most this many line numbers, so a pathological
/// `old_text` cannot produce an unbounded error string.
const MAX_AMBIGUOUS_LINES: usize = 10;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(super) struct EditArgs {
    /// Default file path for entries that omit their own. Relative paths resolve
    /// from the directory where dotlink was launched.
    #[serde(default)]
    path: Option<String>,

    /// Ordered edits. Entries for one file apply to that file's evolving buffer.
    edits: Vec<EditOp>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(super) struct EditOp {
    /// File path for this entry. Falls back to the top-level `path`.
    #[serde(default)]
    path: Option<String>,

    /// Exact text to find. Never include the `N\t` prefix that `read` adds.
    old_text: String,

    /// Replacement text. May be empty.
    new_text: String,

    /// Replace every exact occurrence. Without this flag the match must be unique.
    #[serde(default)]
    replace_all: bool,
}

/// Planning state for one file: the snapshot every entry is applied against and
/// the resulting text.
struct FilePlan {
    snapshot: Snapshot,
    text: String,
    edits: usize,
    /// Replacement texts applied so far, for the substring guard.
    new_texts: Vec<String>,
}

/// A changed file that passed verification and is ready to be written.
struct Changed {
    verified: super::Verified,
    text: String,
    edits: usize,
}

impl LocalMachine {
    async fn edit_impl(&self, args: EditArgs) -> Result<String> {
        let total = args.edits.len();
        if total == 0 {
            anyhow::bail!("edit: edits must not be empty");
        }
        if total > MAX_EDIT_OPS {
            anyhow::bail!("edit: {total} edits, above the {MAX_EDIT_OPS} edit limit");
        }

        let mut combined_bytes = 0_usize;
        let mut resolved_paths = Vec::with_capacity(total);
        for (index, op) in args.edits.iter().enumerate() {
            let position = index + 1;
            combined_bytes = combined_bytes
                .saturating_add(op.old_text.len())
                .saturating_add(op.new_text.len());
            if combined_bytes > self.config.max_write_bytes {
                anyhow::bail!(
                    "edit {position}/{total}: edits are {combined_bytes} bytes, above the {} byte write limit",
                    self.config.max_write_bytes
                );
            }

            let target = op.path.as_deref().or(args.path.as_deref()).ok_or_else(|| {
                anyhow::anyhow!(
                    "edit {position}/{total}: no path (set edits[].path or a top-level path)"
                )
            })?;
            resolved_paths.push(self.resolve_existing(target, AccessNeed::ReadWrite).await?);
        }

        // Hold every target for the whole plan -> verify -> stage -> commit
        // sequence. Locks are sharded by resolved path and acquired in sorted
        // shard order, so multi-file edits cannot deadlock and unrelated files
        // stay concurrent.
        let _mutation = self
            .lock_mutations(resolved_paths.iter().map(|path| path.as_path()))
            .await?;

        let mut files: Vec<FilePlan> = Vec::new();
        for (index, (op, path)) in args.edits.iter().zip(resolved_paths).enumerate() {
            let position = index + 1;
            let slot = match files.iter().position(|file| file.snapshot.path == path) {
                Some(slot) => slot,
                None => {
                    if files.len() >= MAX_EDIT_FILES {
                        anyhow::bail!(
                            "edit {position}/{total}: {} files, above the {MAX_EDIT_FILES} file limit",
                            files.len() + 1
                        );
                    }
                    let display = self.config.access.relative_display(&path);
                    let snapshot = Snapshot::read(self, path, display).await?;
                    let text = snapshot
                        .text()
                        .map_err(|error| anyhow::anyhow!("edit {position}/{total}: {error}"))?
                        .to_owned();
                    files.push(FilePlan {
                        snapshot,
                        text,
                        edits: 0,
                        new_texts: Vec::new(),
                    });
                    files.len() - 1
                }
            };
            let file = &mut files[slot];
            if op.old_text.is_empty() {
                anyhow::bail!(
                    "edit {position}/{total} ({}): old_text must not be empty",
                    file.snapshot.display()
                );
            }
            if file
                .new_texts
                .iter()
                .any(|new_text| new_text.contains(&op.old_text))
            {
                anyhow::bail!(
                    "edit {position}/{total} ({}): old_text is a substring of this edit's predecessor's new_text in the same file",
                    file.snapshot.display()
                );
            }

            let occurrences = match_indices(&file.text, &op.old_text);
            if occurrences.is_empty() {
                if has_line_number_prefix(&op.old_text) {
                    anyhow::bail!(
                        "edit {position}/{total} ({}): old_text appears to include a read line-number prefix (digits then tab); strip it",
                        file.snapshot.display()
                    );
                }
                anyhow::bail!(
                    "edit {position}/{total} ({}): old_text does not occur at the point that edit applies",
                    file.snapshot.display()
                );
            }
            if !op.replace_all && occurrences.len() > 1 {
                let lines = occurrences
                    .iter()
                    .take(MAX_AMBIGUOUS_LINES)
                    .map(|offset| line_number(&file.text, *offset).to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!(
                    "edit {position}/{total} ({}): old_text occurs {} times at lines {lines}; make it unique or set replace_all",
                    file.snapshot.display(),
                    occurrences.len()
                );
            }

            file.text = match op.replace_all {
                true => file.text.replace(&op.old_text, &op.new_text),
                false => file.text.replacen(&op.old_text, &op.new_text, 1),
            };
            file.edits += 1;
            file.new_texts.push(op.new_text.clone());

            if file.text.len() > self.config.max_write_bytes {
                anyhow::bail!(
                    "edit {position}/{total} ({}): edited file would be {} bytes, above the {} byte write limit",
                    file.snapshot.display(),
                    file.text.len(),
                    self.config.max_write_bytes
                );
            }
        }

        // Verify every file before the first write: a file that changed since it
        // was read fails the whole call, and a no-op file is never touched.
        let mut changed: Vec<Changed> = Vec::new();
        for file in files {
            if file.text.as_bytes() == file.snapshot.bytes() {
                continue;
            }
            let verified = file
                .snapshot
                .verify(self, "editing")
                .await
                .map_err(|error| anyhow::anyhow!("{error}; nothing was written"))?;
            changed.push(Changed {
                verified,
                text: file.text,
                edits: file.edits,
            });
        }

        let receipt = serde_json::json!({
            "files": changed
                .iter()
                .map(|change| serde_json::json!({
                    "path": change.verified.display(),
                    "edits": change.edits,
                    // Signed: a shrinking file is a negative delta.
                    "line_delta": line_count(&change.text) as i64
                        - line_count(&String::from_utf8_lossy(change.verified.bytes())) as i64,
                }))
                .collect::<Vec<_>>(),
            "edits_applied": total,
            "changed_files": changed.len(),
        });

        // Stage every file before committing any, so an error while writing
        // one file cannot leave the earlier ones changed. The commit sequence
        // itself is not a multi-file transaction: a rename failure can leave
        // earlier files replaced, and the error says so.
        let mut staged = Vec::with_capacity(changed.len());
        for change in changed {
            let display = change.verified.display().to_owned();
            let pending = change.verified.stage(self, change.text.as_bytes()).await?;
            if let Some(pending) = pending {
                staged.push((display, pending));
            }
        }
        for (display, pending) in staged {
            self.commit_write(pending).await.map_err(|error| {
                anyhow::anyhow!(
                    "{display}: commit failed after staging; earlier files in this edit may already have been replaced: {error}"
                )
            })?;
        }
        Ok(receipt.to_string())
    }
}

#[tool_router(router = "edit_router", vis = "pub(crate)")]
impl LocalMachine {
    #[tool(
        description = "Replace exact text in one or more existing UTF-8 files. `edits` is an ordered array of {path?, old_text, new_text, replace_all?} (at most 16 files and 64 edits per call); `path` is the inherited default. Each old_text must be unique unless replace_all is set, and must not be a substring of an earlier edit's new_text in the same file. `read` prefixes lines with a line number and a tab; never include that prefix in old_text. Every file is planned and re-checked before anything is written, and each file replacement is atomic; the multi-file commit sequence is not a transaction, so a commit failure can leave earlier files replaced. Returns a receipt with per-file path, edits and line_delta, plus edits_applied and changed_files.",
        annotations(
            title = "Edit text files",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn edit(&self, Parameters(args): Parameters<EditArgs>) -> CallToolResult {
        match self.edit_impl(args).await {
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

    fn edit_args(path: Option<&str>, edits: Vec<EditOp>) -> EditArgs {
        EditArgs {
            path: path.map(str::to_owned),
            edits,
        }
    }

    fn edit_op(path: Option<&str>, old_text: &str, new_text: &str) -> EditOp {
        EditOp {
            path: path.map(str::to_owned),
            old_text: old_text.to_owned(),
            new_text: new_text.to_owned(),
            replace_all: false,
        }
    }

    #[tokio::test]
    async fn edit_applies_ordered_multi_file_edits_with_inherited_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("crates/x/src")).unwrap();
        std::fs::create_dir_all(root.join("crates/y/src")).unwrap();
        std::fs::write(root.join("crates/x/src/lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        std::fs::write(root.join("crates/y/src/lib.rs"), "alpha\n").unwrap();
        let machine = test_machine(root.clone());

        let receipt = machine
            .edit_impl(edit_args(
                Some("crates/x/src/lib.rs"),
                vec![
                    edit_op(None, "fn a() {}", "fn a() { one() }"),
                    // Same file, later in the array: applies to the buffer the
                    // first edit produced.
                    edit_op(None, "fn b() {}", "fn b() { two() }"),
                    edit_op(Some("crates/y/src/lib.rs"), "alpha", "beta\ngamma"),
                ],
            ))
            .await
            .unwrap();
        let receipt: Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(receipt["edits_applied"], serde_json::json!(3));
        assert_eq!(receipt["changed_files"], serde_json::json!(2));
        assert_eq!(
            receipt["files"][0],
            serde_json::json!({
                "path": "crates/x/src/lib.rs",
                "edits": 2,
                "line_delta": 0
            })
        );
        assert_eq!(receipt["files"][1]["line_delta"], serde_json::json!(1));
        assert_eq!(
            std::fs::read_to_string(root.join("crates/x/src/lib.rs")).unwrap(),
            "fn a() { one() }\nfn b() { two() }\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("crates/y/src/lib.rs")).unwrap(),
            "beta\ngamma\n"
        );
    }

    #[tokio::test]
    async fn edit_rejects_failures_with_contract_texts() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("crates/x/src")).unwrap();
        std::fs::write(root.join("crates/x/src/lib.rs"), "one\ntwo\ntwo\nthree\n").unwrap();
        std::fs::write(root.join("plain.txt"), "plain\n").unwrap();
        let machine = test_machine(root.clone());
        let target = "crates/x/src/lib.rs";

        // Missing path.
        let error = machine
            .edit_impl(edit_args(None, vec![edit_op(None, "one", "1")]))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "edit 1/1: no path (set edits[].path or a top-level path)"
        );

        // Not found.
        let error = machine
            .edit_impl(edit_args(Some(target), vec![edit_op(None, "nope", "x")]))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "edit 1/1 (crates/x/src/lib.rs): old_text does not occur at the point that edit applies"
        );

        // Ambiguity lists the matching lines.
        let error = machine
            .edit_impl(edit_args(
                Some(target),
                vec![edit_op(None, "one", "1"), edit_op(None, "two", "2")],
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "edit 2/2 (crates/x/src/lib.rs): old_text occurs 2 times at lines 2, 3; make it unique or set replace_all"
        );

        // The read line-number prefix hint.
        let error = machine
            .edit_impl(edit_args(Some(target), vec![edit_op(None, "2\ttwo", "2")]))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "edit 1/1 (crates/x/src/lib.rs): old_text appears to include a read line-number prefix (digits then tab); strip it"
        );

        // old_text that is a substring of the predecessor's new_text.
        let error = machine
            .edit_impl(edit_args(
                Some(target),
                vec![
                    edit_op(None, "one", "one and two"),
                    edit_op(None, "two", "three"),
                ],
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "edit 2/2 (crates/x/src/lib.rs): old_text is a substring of this edit's predecessor's new_text in the same file"
        );

        // Nothing was written by any failed plan.
        assert_eq!(
            std::fs::read_to_string(root.join(target)).unwrap(),
            "one\ntwo\ntwo\nthree\n"
        );

        // A snapshot that no longer matches disk fails before any write.
        let stale = Snapshot {
            path: root.join(target),
            display: target.to_owned(),
            bytes: b"planned".to_vec(),
            limit: 65536,
        };
        let error = stale
            .verify(&machine, "editing")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "crates/x/src/lib.rs changed while editing");
    }

    #[tokio::test]
    async fn edit_replace_all_and_bounds() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "x\ny\nx\n").unwrap();
        let machine = test_machine(root.clone());

        let mut op = edit_op(None, "x", "z");
        op.replace_all = true;
        let receipt = machine
            .edit_impl(edit_args(Some("a.txt"), vec![op]))
            .await
            .unwrap();
        let receipt: Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(receipt["files"][0]["edits"], serde_json::json!(1));
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "z\ny\nz\n"
        );

        let error = machine
            .edit_impl(edit_args(Some("a.txt"), Vec::new()))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("edits must not be empty"), "{error}");

        let too_many = (0..=MAX_EDIT_OPS)
            .map(|index| edit_op(None, &format!("m{index}"), "n"))
            .collect::<Vec<_>>();
        let error = machine
            .edit_impl(edit_args(Some("a.txt"), too_many))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("edit limit"), "{error}");
    }

    #[test]
    fn edit_helpers_detect_line_prefixes_and_numbers() {
        assert!(has_line_number_prefix("12\tfn main()"));
        assert!(!has_line_number_prefix("fn main()"));
        assert!(!has_line_number_prefix("\tfn main()"));
        assert_eq!(line_number("a\nb\nc", 0), 1);
        assert_eq!(line_number("a\nb\nc", 2), 2);
        assert_eq!(line_number("a\nb\nc", 4), 3);
    }

    #[tokio::test]
    async fn edit_line_delta_is_signed_when_a_file_shrinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let machine = test_machine(root.clone());

        let receipt = machine
            .edit_impl(edit_args(Some("a.txt"), vec![edit_op(None, "two\n", "")]))
            .await
            .unwrap();
        let receipt: Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(receipt["files"][0]["line_delta"], serde_json::json!(-1));
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\nthree\n"
        );
    }

    #[tokio::test]
    async fn concurrent_edits_serialize_per_path() {
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
                let old = format!("token{index}");
                let new = format!("CHANGED{index}");
                machine
                    .edit_impl(edit_args(Some("f.txt"), vec![edit_op(None, &old, &new)]))
                    .await
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

    #[test]
    fn the_advertised_description_states_the_enforced_limits() {
        let tool = LocalMachine::all_tools().get("edit").cloned().unwrap();
        let description = tool.description.clone().unwrap_or_default();
        for limit in [MAX_EDIT_FILES, MAX_EDIT_OPS] {
            assert!(
                description.contains(&limit.to_string()),
                "the edit description must advertise its {limit} limit: {description}"
            );
        }
    }

    #[tokio::test]
    async fn edit_skips_writing_a_file_whose_text_did_not_change() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join("a.txt");
        std::fs::write(&path, "same\n").unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let machine = test_machine(root);

        let receipt: Value = serde_json::from_str(
            &machine
                .edit_impl(edit_args(
                    Some("a.txt"),
                    vec![edit_op(None, "same", "same")],
                ))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["edits_applied"], serde_json::json!(1));
        assert_eq!(receipt["changed_files"], serde_json::json!(0));
        assert_eq!(receipt["files"], serde_json::json!([]));
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "a no-op edit must not touch the file"
        );
    }
}
