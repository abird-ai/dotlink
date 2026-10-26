//! `bread`, `bwrite`, `bedit`: binary access with bounded, verified writes.

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, ResourceContents},
    schemars, tool, tool_router,
};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::{LocalMachine, Snapshot, ok_text, tool_error};
use crate::mcp::{AccessNeed, sha256};

/// The largest file any binary tool will hold in memory.
const MAX_PATCH_FILE_BYTES: usize = 64 * 1024 * 1024;
const MAX_BEDIT_RANGES: usize = 32;

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum BreadFormat {
    #[default]
    Mcp,
    Base64,
    Hex,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum BinaryEncoding {
    #[default]
    Base64,
    Hex,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(super) struct BreadArgs {
    /// Binary file path.
    path: String,

    /// "mcp" returns typed MCP image/audio/blob content; base64/hex return encoded text.
    #[serde(default)]
    format: BreadFormat,

    /// Byte offset for base64/hex reads. MCP format requires offset 0.
    #[serde(default)]
    offset: Option<u64>,

    /// Maximum bytes to read. MCP format requires the complete file.
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(super) struct BwriteArgs {
    /// File path.
    path: String,

    /// Encoded binary payload.
    data: String,

    /// Encoding of data.
    #[serde(default)]
    encoding: BinaryEncoding,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(super) struct BeditArgs {
    /// Existing binary file path.
    path: String,

    /// Byte ranges, all relative to the original file. Applied ascending.
    edits: Vec<BeditRange>,

    /// Optional hex SHA-256 of the current file content, in any case. A
    /// mismatch writes nothing.
    #[serde(default)]
    sha256: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(super) struct BeditRange {
    /// Zero-based byte offset in the original file.
    offset: usize,

    /// Number of existing bytes to replace. Defaults to the decoded payload
    /// length. Use 0 to insert; empty data with a positive length deletes.
    #[serde(default)]
    length: Option<usize>,

    /// Replacement bytes encoded as base64 or hex. May be empty.
    #[serde(default)]
    data: String,

    /// Encoding of data.
    #[serde(default)]
    encoding: BinaryEncoding,
}

fn decode_binary(data: &str, encoding: BinaryEncoding, max_bytes: usize) -> Result<Vec<u8>> {
    if data.len() > max_bytes.saturating_mul(2).saturating_add(16) {
        anyhow::bail!("encoded payload is too large");
    }
    let bytes = match encoding {
        BinaryEncoding::Base64 => BASE64.decode(data).context("invalid base64 payload")?,
        BinaryEncoding::Hex => hex::decode(data).context("invalid hex payload")?,
    };
    if bytes.len() > max_bytes {
        anyhow::bail!(
            "decoded payload is {} bytes, above the {} byte limit",
            bytes.len(),
            max_bytes
        );
    }
    Ok(bytes)
}

/// MCP blob URIs are identifiers, not filesystem paths. Encode each path
/// component separately so `#`, `?`, `%`, spaces and Unicode cannot change
/// the URI's query, fragment or path structure.
fn resource_uri(display: &str) -> Result<String> {
    let mut uri = url::Url::parse("abird://file/")?;
    uri.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid MCP resource URI base"))?
        .pop_if_empty()
        .extend(display.split(std::path::MAIN_SEPARATOR));
    Ok(uri.into())
}

/// One validated byte range, kept in request order for messages until sorted.
struct RangePlan {
    offset: usize,
    end: usize,
    length: usize,
    /// 1-based position in the request, used for error text.
    position: usize,
    data: Vec<u8>,
}

impl LocalMachine {
    async fn bread_impl(&self, args: BreadArgs) -> Result<CallToolResult> {
        let path = self.resolve_existing(&args.path, AccessNeed::Read).await?;
        let mut file = self.open_read_file(&path).await?;
        let total_size = file.metadata().await?.len();

        match args.format {
            BreadFormat::Mcp => {
                if args.offset.unwrap_or(0) != 0 {
                    anyhow::bail!("format=mcp requires offset=0");
                }
                let limit = args.limit.unwrap_or(self.config.max_read_bytes);
                if total_size > limit.min(self.config.max_read_bytes) as u64 {
                    anyhow::bail!(
                        "format=mcp requires the complete file within the read limit; use base64 or hex for chunked reads"
                    );
                }
                let bytes =
                    Self::read_bounded(&mut file, limit.min(self.config.max_read_bytes)).await?;
                let encoded = BASE64.encode(bytes);
                let mime = mime_guess::from_path(&path)
                    .first_or_octet_stream()
                    .essence_str()
                    .to_owned();
                let content = if mime.starts_with("image/") {
                    ContentBlock::image(encoded, mime)
                } else if mime.starts_with("audio/") {
                    ContentBlock::audio(encoded, mime)
                } else {
                    let uri = resource_uri(&self.config.access.relative_display(&path))?;
                    ContentBlock::resource(
                        ResourceContents::blob(encoded, uri).with_mime_type(mime),
                    )
                };
                Ok(CallToolResult::success(vec![content]))
            }
            BreadFormat::Base64 | BreadFormat::Hex => {
                let offset = args.offset.unwrap_or(0);
                if offset > total_size {
                    anyhow::bail!("offset is beyond end of file");
                }
                let limit = args
                    .limit
                    .unwrap_or(self.config.max_read_bytes)
                    .clamp(1, self.config.max_read_bytes);
                file.seek(std::io::SeekFrom::Start(offset)).await?;
                let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
                file.take(limit as u64).read_to_end(&mut bytes).await?;
                let (format, encoded) = match args.format {
                    BreadFormat::Base64 => ("base64", BASE64.encode(&bytes)),
                    BreadFormat::Hex => ("hex", hex::encode(&bytes)),
                    BreadFormat::Mcp => unreachable!(),
                };
                let truncated = offset + (bytes.len() as u64) < total_size;
                Ok(ok_text(
                    serde_json::json!({
                        "path": args.path,
                        "format": format,
                        "offset": offset,
                        "bytes": bytes.len(),
                        "total_size": total_size,
                        "truncated": truncated,
                        "data": encoded,
                    })
                    .to_string(),
                ))
            }
        }
    }

    async fn bwrite_impl(&self, args: BwriteArgs) -> Result<String> {
        let bytes = decode_binary(&args.data, args.encoding, self.config.max_write_bytes)?;
        let path = self
            .resolve_for_create(&args.path, AccessNeed::Write)
            .await?;
        let _mutation = self.lock_mutations([path.as_path()]).await?;
        self.write_file_atomic(&path, &bytes).await?;
        Ok(format!("wrote {} bytes to {}", bytes.len(), args.path))
    }

    /// Multi-range binary edit against original-file offsets. Ranges are decoded
    /// and validated before the file is rewritten, the optional `sha256`
    /// precondition is enforced against fresh bytes at write time, and the
    /// replacement is atomic: the file is never truncated in place.
    async fn bedit_impl(&self, args: BeditArgs) -> Result<String> {
        let total = args.edits.len();
        if total == 0 {
            anyhow::bail!("bedit: edits must not be empty");
        }
        if total > MAX_BEDIT_RANGES {
            anyhow::bail!("bedit: {total} ranges, above the {MAX_BEDIT_RANGES} range limit");
        }
        let path = self
            .resolve_existing(&args.path, AccessNeed::ReadWrite)
            .await?;
        let _mutation = self.lock_mutations([path.as_path()]).await?;
        let display = self.config.access.relative_display(&path);
        // `max_write_bytes` is the profile's write ceiling; the larger constant
        // is only a hard upper bound for binary planning memory.
        let limit = self.config.max_write_bytes.min(MAX_PATCH_FILE_BYTES);
        let snapshot = Snapshot::read_within(self, path, display.clone(), limit)
            .await
            .map_err(|error| {
                anyhow::anyhow!("{error} (bedit accepts files up to {limit} bytes)")
            })?;
        let size_before = snapshot.bytes().len();
        if let Some(expected) = args.sha256.as_deref() {
            let found = hex::encode(sha256(snapshot.bytes()));
            if !expected.eq_ignore_ascii_case(&found) {
                anyhow::bail!("bedit: sha256 mismatch (expected {expected}, found {found})");
            }
        }

        // Decoded payloads are held until the plan is valid, so the running
        // total is bounded: no request can pin more than one patchable file
        // worth of bytes in memory while its ranges are validated.
        let mut decoded_bytes = 0_usize;
        let mut ranges = Vec::with_capacity(total);
        for (index, range) in args.edits.iter().enumerate() {
            let position = index + 1;
            if range.offset > size_before {
                anyhow::bail!(
                    "bedit {position}/{total}: offset {} is beyond end of file ({size_before} bytes)",
                    range.offset
                );
            }
            let data = decode_binary(&range.data, range.encoding, limit)
                .with_context(|| format!("bedit {position}/{total}"))?;
            decoded_bytes = decoded_bytes.saturating_add(data.len());
            if decoded_bytes > limit {
                anyhow::bail!(
                    "bedit: replacement payloads total {decoded_bytes} bytes, above the {limit} byte limit"
                );
            }
            let length = range.length.unwrap_or(data.len());
            let Some(end) = range
                .offset
                .checked_add(length)
                .filter(|end| *end <= size_before)
            else {
                anyhow::bail!(
                    "bedit {position}/{total}: range extends beyond end of file ({size_before} bytes)"
                );
            };
            ranges.push(RangePlan {
                offset: range.offset,
                end,
                length,
                position,
                data,
            });
        }

        // Deterministic order: by offset, then by length so an insert at a
        // boundary always precedes a replacement that starts there, then by
        // request position so two ranges with the same span apply in the order
        // the caller listed them.
        ranges.sort_by_key(|range| (range.offset, range.length, range.position));
        for (index, range) in ranges.iter().enumerate() {
            let Some(previous) = index.checked_sub(1).map(|index| &ranges[index]) else {
                continue;
            };
            if range.offset < previous.end {
                anyhow::bail!(
                    "bedit {}/{total}: range overlaps range {} (offset {}, length {})",
                    range.position,
                    previous.position,
                    previous.offset,
                    previous.length
                );
            }
        }

        let bytes_written = ranges.iter().map(|range| range.data.len()).sum::<usize>();
        let bytes_removed = ranges
            .iter()
            .map(|range| range.end - range.offset)
            .sum::<usize>();
        let size_after = size_before - bytes_removed + bytes_written;
        if size_after > limit {
            anyhow::bail!("bedit: patched file would exceed the {limit} byte limit");
        }

        let mut edited = Vec::with_capacity(size_after);
        let mut cursor = 0_usize;
        for range in &ranges {
            edited.extend_from_slice(&snapshot.bytes()[cursor..range.offset]);
            edited.extend_from_slice(&range.data);
            cursor = range.end;
        }
        edited.extend_from_slice(&snapshot.bytes()[cursor..]);
        debug_assert_eq!(edited.len(), size_after);

        let verified = snapshot
            .verify(self, "editing")
            .await
            .map_err(|error| anyhow::anyhow!("{error}; nothing was written"))?;
        let changed = verified
            .commit(self, &edited)
            .await
            .map_err(|error| anyhow::anyhow!("{error}; nothing was written"))?;
        Ok(serde_json::json!({
            "path": display,
            "changed": changed,
            "ranges": total,
            "bytes_written": if changed { bytes_written } else { 0 },
            "size_before": size_before,
            "size_after": size_after,
        })
        .to_string())
    }
}

#[tool_router(router = "binary_router", vis = "pub(crate)")]
impl LocalMachine {
    #[tool(
        description = "Read a binary file. format=mcp returns typed MCP image/audio/blob content for the complete file; base64 or hex returns encoded bytes for a byte window with offset/limit.",
        annotations(
            title = "Read binary file",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn bread(&self, Parameters(args): Parameters<BreadArgs>) -> CallToolResult {
        match self.bread_impl(args).await {
            Ok(result) => result,
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Create or replace a binary file atomically using base64 or hex data.",
        annotations(
            title = "Write binary file",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn bwrite(&self, Parameters(args): Parameters<BwriteArgs>) -> CallToolResult {
        match self.bwrite_impl(args).await {
            Ok(receipt) => ok_text(receipt),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Edit byte ranges in an existing binary file, all or nothing. `edits` is an array of {offset, length?, data, encoding}, at most 32 ranges; offsets are relative to the original file, ranges are applied ascending (insert before replace at one offset, request order otherwise), and overlaps are rejected. `length` defaults to the decoded payload size, 0 inserts, and empty data with a positive length deletes. An optional hex `sha256` precondition is re-checked against the file before writing, and the write is atomic. Returns a receipt with path, changed, ranges, bytes_written, size_before and size_after; changed is false when the result equals the original and nothing was written.",
        annotations(
            title = "Edit binary file",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn bedit(&self, Parameters(args): Parameters<BeditArgs>) -> CallToolResult {
        match self.bedit_impl(args).await {
            Ok(receipt) => ok_text(receipt),
            Err(error) => tool_error(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_resource_uri_encodes_each_path_segment() {
        for (input, suffix) in [
            ("simple.bin", "simple.bin"),
            ("report #1.bin", "report%20%231.bin"),
            ("report?tab.bin", "report%3Ftab.bin"),
            ("100%.bin", "100%25.bin"),
            ("pi=π.bin", "pi=%CF%80.bin"),
            (
                "nested/folder #1/file?x%.bin",
                "nested/folder%20%231/file%3Fx%25.bin",
            ),
        ] {
            // On Windows the platform's separator is backslash.
            let platform = input.replace('/', std::path::MAIN_SEPARATOR_STR);
            let uri = resource_uri(&platform).unwrap();
            assert_eq!(uri, format!("abird://file/{suffix}"), "{input:?}");
            let parsed = url::Url::parse(&uri).unwrap();
            assert!(parsed.query().is_none() && parsed.fragment().is_none());
        }
    }

    use crate::mcp::tools::test_support::test_machine;
    use serde_json::Value;

    fn bedit_range(offset: usize, length: Option<usize>, data: &str) -> BeditRange {
        BeditRange {
            offset,
            length,
            data: BASE64.encode(data.as_bytes()),
            encoding: BinaryEncoding::Base64,
        }
    }

    #[tokio::test]
    async fn bedit_applies_multi_range_edits_against_original_offsets() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("data.bin"), b"0123456789").unwrap();
        let machine = test_machine(root.clone());

        let receipt = machine
            .bedit_impl(BeditArgs {
                path: "data.bin".to_owned(),
                // Out of order on purpose: ranges apply ascending by offset
                // while every offset stays relative to the original file.
                edits: vec![
                    bedit_range(6, Some(2), "ab"),
                    bedit_range(1, Some(2), ""),
                    bedit_range(9, Some(1), "XYZ"),
                ],
                sha256: None,
            })
            .await
            .unwrap();
        let receipt: Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(receipt["path"], "data.bin");
        assert_eq!(receipt["ranges"], serde_json::json!(3));
        assert_eq!(receipt["bytes_written"], serde_json::json!(5));
        assert_eq!(receipt["size_before"], serde_json::json!(10));
        assert_eq!(receipt["size_after"], serde_json::json!(10));
        assert_eq!(std::fs::read(root.join("data.bin")).unwrap(), b"0345ab8XYZ");
    }

    #[tokio::test]
    async fn bedit_insert_delete_and_sha256_precondition() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("data.bin"), b"abcdef").unwrap();
        let machine = test_machine(root.clone());
        let digest = hex::encode(sha256(b"abcdef"));

        machine
            .bedit_impl(BeditArgs {
                path: "data.bin".to_owned(),
                edits: vec![bedit_range(0, Some(0), ">>"), bedit_range(2, Some(2), "")],
                sha256: Some(digest.clone()),
            })
            .await
            .unwrap();
        assert_eq!(std::fs::read(root.join("data.bin")).unwrap(), b">>abef");

        let error = machine
            .bedit_impl(BeditArgs {
                path: "data.bin".to_owned(),
                edits: vec![bedit_range(0, Some(0), "x")],
                sha256: Some(digest),
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("bedit: sha256 mismatch (expected "),
            "{error}"
        );
        assert_eq!(std::fs::read(root.join("data.bin")).unwrap(), b">>abef");
    }

    #[tokio::test]
    async fn bedit_rejects_overlaps_and_out_of_bounds_ranges() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("data.bin"), vec![b'x'; 512]).unwrap();
        let machine = test_machine(root.clone());

        let error = machine
            .bedit_impl(BeditArgs {
                path: "data.bin".to_owned(),
                edits: vec![bedit_range(0, Some(16), "a"), bedit_range(8, Some(16), "b")],
                sha256: None,
            })
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "bedit 2/2: range overlaps range 1 (offset 0, length 16)"
        );

        let error = machine
            .bedit_impl(BeditArgs {
                path: "data.bin".to_owned(),
                edits: vec![bedit_range(999, Some(1), "a")],
                sha256: None,
            })
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "bedit 1/1: offset 999 is beyond end of file (512 bytes)"
        );

        let error = machine
            .bedit_impl(BeditArgs {
                path: "data.bin".to_owned(),
                edits: vec![bedit_range(510, Some(16), "a")],
                sha256: None,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("beyond end of file"), "{error}");

        let too_many = (0..=MAX_BEDIT_RANGES)
            .map(|offset| bedit_range(offset, Some(0), ""))
            .collect::<Vec<_>>();
        let error = machine
            .bedit_impl(BeditArgs {
                path: "data.bin".to_owned(),
                edits: too_many,
                sha256: None,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("range limit"), "{error}");

        // Nothing was written by any rejected plan.
        assert_eq!(
            std::fs::read(root.join("data.bin")).unwrap(),
            vec![b'x'; 512]
        );
    }

    #[tokio::test]
    async fn bedit_orders_inserts_before_replacements_at_the_same_offset() {
        // The same range set must produce the same file whatever order the
        // request lists it in: an insert at an offset precedes the replacement
        // that starts there.
        for ranges in [
            vec![bedit_range(2, Some(0), "XY"), bedit_range(2, Some(2), "")],
            vec![bedit_range(2, Some(2), ""), bedit_range(2, Some(0), "XY")],
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            std::fs::write(root.join("data.bin"), b"0123456789").unwrap();
            let machine = test_machine(root.clone());
            machine
                .bedit_impl(BeditArgs {
                    path: "data.bin".to_owned(),
                    edits: ranges,
                    sha256: None,
                })
                .await
                .unwrap();
            assert_eq!(
                std::fs::read(root.join("data.bin")).unwrap(),
                b"01XY456789",
                "insert then delete 2..4"
            );
        }
    }

    #[tokio::test]
    async fn bedit_rechecks_its_precondition_against_fresh_bytes() {
        // A stale snapshot is the only thing the precondition can catch here;
        // the point is that the write path re-reads rather than trusting the
        // bytes it planned against.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("data.bin"), b"abcdef").unwrap();
        let machine = test_machine(root.clone());

        let mut args = BeditArgs {
            path: "data.bin".to_owned(),
            edits: vec![bedit_range(0, Some(1), "X")],
            sha256: None,
        };
        // Correct hash of the old content, then a rewrite behind its back.
        args.sha256 = Some(hex::encode(crate::mcp::sha256(b"abcdef")));
        std::fs::write(root.join("data.bin"), b"changed").unwrap();
        let error = machine.bedit_impl(args).await.unwrap_err().to_string();
        assert!(error.contains("sha256 mismatch"), "{error}");
        assert_eq!(
            std::fs::read(root.join("data.bin")).unwrap(),
            b"changed",
            "a failed precondition writes nothing"
        );
    }

    #[tokio::test]
    async fn bedit_applies_same_offset_inserts_in_request_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("data.bin"), b"0123456789").unwrap();
        let machine = test_machine(root.clone());

        machine
            .bedit_impl(BeditArgs {
                path: "data.bin".to_owned(),
                edits: vec![bedit_range(2, Some(0), "AA"), bedit_range(2, Some(0), "BB")],
                sha256: None,
            })
            .await
            .unwrap();
        // Same span at one offset: the request order breaks the tie.
        assert_eq!(
            std::fs::read(root.join("data.bin")).unwrap(),
            b"01AABB23456789"
        );
    }

    #[tokio::test]
    async fn bedit_leaves_the_file_intact_when_the_plan_is_invalid() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("data.bin"), b"abcdef").unwrap();
        let machine = test_machine(root.clone());
        let error = machine
            .bedit_impl(BeditArgs {
                path: "data.bin".to_owned(),
                edits: vec![bedit_range(0, Some(4), "XY"), bedit_range(2, Some(4), "xx")],
                sha256: None,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("overlaps"), "{error}");
        assert_eq!(std::fs::read(root.join("data.bin")).unwrap(), b"abcdef");
    }

    #[tokio::test]
    async fn concurrent_bedit_calls_serialize_per_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("data.bin"), vec![b'a'; 32 * 1024]).unwrap();
        let machine = std::sync::Arc::new(test_machine(root.clone()));
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..8 {
            let machine = machine.clone();
            tasks.spawn(async move {
                let payload = ((b'A' + index) as char).to_string();
                machine
                    .bedit_impl(BeditArgs {
                        path: "data.bin".to_owned(),
                        edits: vec![bedit_range(index as usize, Some(1), &payload)],
                        sha256: None,
                    })
                    .await
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap().unwrap();
        }
        let bytes = std::fs::read(root.join("data.bin")).unwrap();
        for (index, byte) in bytes.iter().enumerate().take(8) {
            assert_eq!(*byte, b'A' + index as u8);
        }
    }

    #[tokio::test]
    async fn bedit_respects_the_profile_write_limit() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("data.bin"), vec![b'a'; 10 * 1024]).unwrap();
        let machine = crate::mcp::tools::test_support::test_machine_with(root.clone(), |config| {
            config.max_write_bytes = 1024
        });
        let error = machine
            .bedit_impl(BeditArgs {
                path: "data.bin".to_owned(),
                edits: vec![bedit_range(0, Some(1), "Z")],
                sha256: None,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("1024 byte"), "{error}");
        assert_eq!(std::fs::read(root.join("data.bin")).unwrap()[0], b'a');
    }

    #[test]
    fn the_advertised_description_states_the_enforced_limits() {
        let tool = LocalMachine::all_tools().get("bedit").cloned().unwrap();
        let description = tool.description.clone().unwrap_or_default();
        assert!(
            description.contains(&MAX_BEDIT_RANGES.to_string()),
            "the bedit description must advertise its {MAX_BEDIT_RANGES} range limit: {description}"
        );
    }

    #[tokio::test]
    async fn bedit_reports_a_no_op_without_claiming_a_write() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join("data.bin");
        std::fs::write(&path, b"abcdef").unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let machine = test_machine(root);

        let receipt: Value = serde_json::from_str(
            &machine
                .bedit_impl(BeditArgs {
                    path: "data.bin".to_owned(),
                    edits: vec![bedit_range(0, Some(3), "abc")],
                    sha256: None,
                })
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["changed"], Value::Bool(false));
        assert_eq!(receipt["bytes_written"], serde_json::json!(0));
        assert_eq!(std::fs::read(&path).unwrap(), b"abcdef");
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
    }
}
