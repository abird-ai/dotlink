//! `read` and `write`: UTF-8 text, numbered windows and atomic replacement.

use std::io;

use anyhow::Result;
use rmcp::{
    RoleServer,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Tool},
    schemars,
    service::RequestContext,
    tool, tool_router,
};
use serde::Deserialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};

use super::{
    LocalMachine, marker_lines, mutation::compact_text_receipt, ok_text, resolve_capped, tool_error,
};
use crate::mcp::AccessNeed;

const DEFAULT_READ_LINES: usize = 4 * 1024;
/// Limit work before a requested line window, independently of output size.
const MAX_READ_SCAN_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(super) struct ReadArgs {
    /// UTF-8 text file path. Relative paths resolve from the directory where dotlink was launched.
    path: String,

    /// First 1-based line to return. Defaults to 1.
    #[serde(default)]
    offset: Option<usize>,

    /// Maximum number of lines to return. Defaults to 4096, bounded by the profile
    /// read limit. A capped result ends with one `… truncated at line N` marker.
    #[serde(default)]
    limit: Option<usize>,

    /// Prefix each line with its 1-based line number and a tab. Defaults to true.
    /// Set false to return the file text exactly, byte for byte.
    #[serde(default)]
    numbered: Option<bool>,

    /// Maximum returned bytes for this call. Defaults to the profile read ceiling
    /// and is capped there. A capped result ends with a truncation marker.
    #[serde(default)]
    max_read_bytes: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(super) struct WriteArgs {
    /// File path. Relative paths resolve from the directory where dotlink was launched.
    path: String,

    /// UTF-8 content. Missing parent directories are created automatically.
    content: String,
}

/// A line without its terminator, for numbered rendering.
fn line_body(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

/// Read one line without allowing a missing newline to allocate past `limit`.
async fn read_line_bounded<R>(reader: &mut R, line: &mut Vec<u8>, limit: usize) -> io::Result<bool>
where
    R: AsyncBufRead + Unpin,
{
    line.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(!line.is_empty());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(take) > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a line exceeds the {limit} byte read limit"),
            ));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            return Ok(true);
        }
    }
}

/// Skip one line without buffering it. `Take` limits actual source I/O, so a
/// `BufReader` fill can never prefetch past the scan budget. Its buffer remains
/// in place when the limit is lifted for the selected window.
async fn skip_line<R>(reader: &mut BufReader<tokio::io::Take<R>>) -> io::Result<bool>
where
    R: AsyncRead + Unpin,
{
    let mut consumed = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if reader.get_ref().limit() == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "read scan budget exceeded; narrow the file or use grep to locate the text",
                ));
            }
            // An unterminated final line still counts as a line.
            return Ok(consumed);
        }
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        // Even skipped lines cannot silently cross a NUL-containing binary
        // prefix. No buffering or UTF-8 decoding of skipped lines is required.
        if super::has_binary_control(&available[..end]) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "binary file; use bread",
            ));
        }
        let newline = end < available.len() || available.get(end - 1) == Some(&b'\n');
        reader.consume(end);
        if newline {
            return Ok(true);
        }
        consumed = true;
    }
}

/// Position a buffered reader at `offset` under an exact source-I/O ceiling.
/// Lifting `Take`'s limit in place retains bytes prefetched from the selected
/// line and lets the ordinary page reader continue from the same buffer.
async fn reader_at_line<R>(
    source: R,
    offset: usize,
    scan_budget: usize,
) -> Result<(BufReader<tokio::io::Take<R>>, usize)>
where
    R: AsyncRead + Unpin,
{
    let offset = offset.max(1);
    let mut reader = BufReader::new(source.take(scan_budget as u64));
    let mut line_number = 0_usize;
    while line_number + 1 < offset {
        if !skip_line(&mut reader)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?
        {
            break;
        }
        line_number += 1;
    }
    if line_number + 1 < offset {
        anyhow::bail!("offset {offset} is beyond end of file ({line_number} lines)");
    }
    reader.get_mut().set_limit(u64::MAX);
    Ok((reader, line_number))
}

/// Drop the last whole line of a buffer that holds only lines. Each line ends
/// with its terminator, except possibly the file's last one.
pub(crate) fn with_read_limit(tool: &mut Tool, max_bytes: usize) {
    if tool.name.as_ref() != "read" {
        return;
    }
    let mut schema = (*tool.input_schema).clone();
    if let Some(property) = schema
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|properties| properties.get_mut("max_read_bytes"))
        .and_then(serde_json::Value::as_object_mut)
    {
        property.insert(
            "description".to_owned(),
            serde_json::Value::String(format!(
                "Maximum raw returned bytes for this call. Defaults to {max_bytes} and is capped at {max_bytes}. When the transport supplies a serialized-result budget, the final page also fits that budget, so escape-heavy text may end earlier at a whole-line boundary."
            )),
        );
        property.insert("maximum".to_owned(), serde_json::Value::from(max_bytes));
    }
    tool.input_schema = std::sync::Arc::new(schema);
}

impl LocalMachine {
    #[cfg(test)]
    async fn read_impl(&self, args: ReadArgs) -> Result<String> {
        self.read_impl_with_result_budget(args, None).await
    }

    async fn read_impl_with_result_budget(
        &self,
        args: ReadArgs,
        result_budget: Option<usize>,
    ) -> Result<String> {
        self.read_impl_with_limits(args, MAX_READ_SCAN_BYTES, result_budget)
            .await
    }

    #[cfg(test)]
    async fn read_impl_with_scan_budget(
        &self,
        args: ReadArgs,
        scan_budget: usize,
    ) -> Result<String> {
        self.read_impl_with_limits(args, scan_budget, None).await
    }

    async fn read_impl_with_limits(
        &self,
        args: ReadArgs,
        scan_budget: usize,
        result_budget: Option<usize>,
    ) -> Result<String> {
        let path = self.resolve_existing(&args.path, AccessNeed::Read).await?;
        let file = self.open_read_file(&path).await?;

        let offset = args.offset.unwrap_or(1).max(1);
        // A zero window would render a nonsense marker ("truncated at line 0").
        let limit = args.limit.unwrap_or(DEFAULT_READ_LINES).max(1);
        let numbered = args.numbered.unwrap_or(true);
        let byte_limit = resolve_capped(args.max_read_bytes, self.config.max_read_bytes);
        let (mut reader, mut line_number) = reader_at_line(file, offset, scan_budget).await?;
        // A trailing newline terminates the preceding line; it does not create
        // another line. Only an actually empty file is readable at offset 1.
        if reader.fill_buf().await?.is_empty() {
            if offset == 1 {
                return Ok(String::new());
            }
            anyhow::bail!("offset {offset} is beyond end of file ({line_number} lines)");
        }

        // One output buffer with whole lines only, so making room for the
        // truncation marker can drop a line but never cut one in half.
        let mut out: Vec<u8> = Vec::new();
        let mut line_ends = Vec::with_capacity(limit.min(DEFAULT_READ_LINES));
        let mut line = Vec::new();
        let mut emitted = 0_usize;
        let mut capped = false;
        while emitted < limit {
            if !read_line_bounded(&mut reader, &mut line, byte_limit)
                .await
                .map_err(|error| anyhow::anyhow!("line {}: {error}", line_number + 1))?
            {
                break;
            }
            // Only returned lines are UTF-8 decoded; skipped lines are
            // checked for binary controls without buffering their contents.
            let text = super::require_text_bytes(&line, "bread")
                .map_err(|_| anyhow::anyhow!("binary file; use bread"))?;
            let number = (line_number + 1).to_string();
            let body = if numbered { line_body(text) } else { text };
            let rendered_len = if numbered {
                number.len() + 1 + body.len() + 1
            } else {
                body.len()
            };
            if out.len().saturating_add(rendered_len) > byte_limit {
                // The line does not fit the ceiling; the page ends before it.
                capped = true;
                break;
            }
            if numbered {
                out.extend_from_slice(number.as_bytes());
                out.push(b'\t');
                out.extend_from_slice(body.as_bytes());
                out.push(b'\n');
            } else {
                out.extend_from_slice(body.as_bytes());
            }
            line_number += 1;
            emitted += 1;
            line_ends.push(out.len());
        }
        if !capped && emitted == limit {
            // Peek only the next buffer, never the next whole line: a huge
            // following line must not fail a window that already fits.
            capped = !reader.fill_buf().await?.is_empty();
        }

        // A page that cannot hold its own first line would otherwise render
        // "truncated at line 0": report the line and the remedy instead.
        if capped && emitted == 0 {
            anyhow::bail!(
                "line {offset} does not fit the {byte_limit} byte read limit; raise max_read_bytes or read fewer numbered lines"
            );
        }

        // The returned text is what `max_read_bytes` bounds, marker included.
        // Drop whole lines until the marker fits beside them.
        if capped {
            let mut marker = marker_lines(offset + emitted - 1);
            while out.len() + 1 + marker.len() > byte_limit {
                line_ends.pop();
                emitted -= 1;
                let end = line_ends.last().copied().unwrap_or(0);
                out.truncate(end);
                if emitted == 0 {
                    anyhow::bail!(
                        "line {offset} and its truncation marker do not fit the {byte_limit} byte read limit; raise max_read_bytes"
                    );
                }
                marker = marker_lines(offset + emitted - 1);
            }
        }

        // Raw bytes and serialized MCP bytes are different budgets: JSON
        // escaping can expand quotes/newlines substantially. Keep the 8 MiB raw
        // profile ceiling, but fit the actual text result below the shared MCP
        // result budget by dropping whole trailing lines only when necessary.
        let result_budget = result_budget.unwrap_or(usize::MAX);
        let current =
            std::str::from_utf8(&out).map_err(|_| anyhow::anyhow!("binary file; use bread"))?;
        let plain = if !capped && numbered && current.ends_with('\n') {
            &current[..current.len() - 1]
        } else {
            current
        };
        let current_size = if capped {
            let marker = marker_lines(offset + emitted - 1);
            let separator = if current.is_empty() || current.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            crate::mcp::serialized_text_tool_result_parts_len(&[current, separator, &marker])
        } else {
            crate::mcp::serialized_text_tool_result_len(plain)
        };
        if current_size > result_budget {
            capped = true;
            if emitted == 0 {
                anyhow::bail!("read result cannot fit the MCP response budget");
            }
            let mut low = 1_usize;
            let mut high = emitted;
            let mut best = 0_usize;
            while low <= high {
                let mid = low + (high - low) / 2;
                let end = line_ends[mid - 1];
                let prefix = std::str::from_utf8(&out[..end])
                    .map_err(|_| anyhow::anyhow!("binary file; use bread"))?;
                let marker = marker_lines(offset + mid - 1);
                let separator = if prefix.is_empty() || prefix.ends_with('\n') {
                    ""
                } else {
                    "\n"
                };
                let size = crate::mcp::serialized_text_tool_result_parts_len(&[
                    prefix, separator, &marker,
                ]);
                if size <= result_budget {
                    best = mid;
                    low = mid + 1;
                } else if mid == 1 {
                    break;
                } else {
                    high = mid - 1;
                }
            }
            if best == 0 {
                anyhow::bail!(
                    "line {offset} and its truncation marker do not fit the MCP response budget; use bread for this content"
                );
            }
            emitted = best;
            out.truncate(line_ends[best - 1]);
            line_ends.truncate(best);
        }

        let mut text =
            String::from_utf8(out).map_err(|_| anyhow::anyhow!("binary file; use bread"))?;
        if numbered && text.ends_with('\n') {
            text.pop();
        }
        if capped {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&marker_lines(offset + emitted - 1));
        }
        Ok(text)
    }

    async fn write_impl(&self, args: WriteArgs) -> Result<String> {
        super::require_text_bytes(args.content.as_bytes(), "bwrite")?;
        if args.content.len() > self.config.max_write_bytes {
            anyhow::bail!(
                "content is {} bytes, above the {} byte write limit",
                args.content.len(),
                self.config.max_write_bytes
            );
        }
        let reply = compact_text_receipt(
            format!("wrote {} bytes to {}", args.content.len(), args.path),
            format!("wrote {} bytes (path omitted)", args.content.len()),
            self.config.max_output_bytes,
        )?;
        let lexical = self.config.access.lexical_target(&args.path)?;
        let _name_admission = self.reserve_mutation_names([lexical.as_path()]).await?;
        let path = self
            .resolve_for_create(&args.path, AccessNeed::Write)
            .await?;
        let _mutation = self.lock_mutations([path.as_path()]).await?;
        self.write_file_atomic(&path, args.content.as_bytes())
            .await?;
        Ok(reply)
    }
}

#[tool_router(router = "text_router", vis = "pub(crate)")]
impl LocalMachine {
    #[tool(
        description = "Read a UTF-8 text file as numbered lines. `offset` is the 1-based first line (default 1), `limit` the line count (default 4096), and each line is prefixed with its number and a tab unless `numbered` is false, which returns the text exactly. A capped window ends with one truncation marker. Skipping to an offset scans at most 32 MiB per call; use grep or a narrower file if exceeded. An offset past the end of the file is an error.",
        annotations(
            title = "Read text file",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn read(
        &self,
        Parameters(args): Parameters<ReadArgs>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        match self
            .read_impl_with_result_budget(args, self.request_result_budget(&context))
            .await
        {
            Ok(text) => ok_text(text),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Create or replace a UTF-8 text file atomically. Missing parent directories are created automatically. The file is either the previous content or the new content, never a partial write.",
        annotations(
            title = "Write text file",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn write(&self, Parameters(args): Parameters<WriteArgs>) -> CallToolResult {
        match self.write_impl(args).await {
            Ok(receipt) => ok_text(receipt),
            Err(error) => tool_error(error),
        }
    }
}

#[cfg(test)]
mod tests {
    impl LocalMachine {
        async fn read_for_test(&self, Parameters(args): Parameters<ReadArgs>) -> CallToolResult {
            match self.read_impl(args).await {
                Ok(text) => ok_text(text),
                Err(error) => tool_error(error),
            }
        }
    }

    #[test]
    fn read_schema_exposes_the_profile_byte_ceiling() {
        let mut tool = LocalMachine::all_tools().get("read").cloned().unwrap();
        with_read_limit(&mut tool, 8 * 1024 * 1024);
        let property = &tool.input_schema["properties"]["max_read_bytes"];
        assert_eq!(property["maximum"], 8 * 1024 * 1024);
        assert!(
            property["description"]
                .as_str()
                .unwrap()
                .contains("8388608")
        );
    }

    use super::*;
    use crate::mcp::tools::test_support::{test_machine, test_machine_with};

    #[tokio::test]
    async fn skipped_read_bytes_have_a_separate_work_budget() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("lines.txt"), b"alpha\nbeta\ngamma\n").unwrap();
        let machine = test_machine(root);
        let args = || ReadArgs {
            path: "lines.txt".to_owned(),
            offset: Some(3),
            limit: Some(1),
            numbered: Some(false),

            max_read_bytes: None,
        };
        let error = machine
            .read_impl_with_scan_budget(args(), 10)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("read scan budget exceeded"),
            "{error}"
        );
        assert_eq!(
            machine
                .read_impl_with_scan_budget(args(), 11)
                .await
                .unwrap(),
            "gamma\n"
        );
    }

    #[tokio::test]
    async fn scan_budget_caps_source_io_without_losing_prefetched_window_bytes() {
        use std::{
            pin::Pin,
            sync::{Arc, Mutex},
            task::{Context, Poll},
        };
        use tokio::io::ReadBuf;

        #[derive(Debug)]
        struct CountingReader {
            bytes: Vec<u8>,
            position: usize,
            reads: Arc<Mutex<Vec<usize>>>,
        }

        impl AsyncRead for CountingReader {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buffer: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                let available = &self.bytes[self.position..];
                let count = available.len().min(buffer.remaining());
                if count > 0 {
                    buffer.put_slice(&available[..count]);
                    self.position += count;
                    self.reads.lock().unwrap().push(count);
                }
                Poll::Ready(Ok(()))
            }
        }

        let source = b"alpha\nbeta\ngamma\n";
        let reads = Arc::new(Mutex::new(Vec::new()));
        let error = reader_at_line(
            CountingReader {
                bytes: source.to_vec(),
                position: 0,
                reads: reads.clone(),
            },
            3,
            10,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("read scan budget exceeded"));
        assert_eq!(reads.lock().unwrap().iter().sum::<usize>(), 10);

        let reads = Arc::new(Mutex::new(Vec::new()));
        let (mut reader, line_number) = reader_at_line(
            CountingReader {
                bytes: source.to_vec(),
                position: 0,
                reads: reads.clone(),
            },
            3,
            16,
        )
        .await
        .unwrap();
        assert_eq!(line_number, 2);
        assert_eq!(reads.lock().unwrap().iter().sum::<usize>(), 16);

        let mut line = Vec::new();
        assert!(
            read_line_bounded(&mut reader, &mut line, 1024)
                .await
                .unwrap()
        );
        assert_eq!(line, b"gamma\n");
        // Five selected-line bytes were already buffered under the cap; only
        // its final newline required more source I/O after lifting the limit.
        assert_eq!(reads.lock().unwrap().iter().sum::<usize>(), 17);
    }

    #[tokio::test]
    async fn read_refuses_to_replace_the_only_line_with_line_zero_marker() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(
            root.join("long.txt"),
            format!("{}\nnext\n", "a".repeat(1008)),
        )
        .unwrap();
        let machine = test_machine_with(root, |config| {
            config.max_read_bytes = 1024;
        });
        let result = machine
            .read_impl(ReadArgs {
                path: "long.txt".into(),
                offset: None,
                limit: Some(1),
                numbered: None,

                max_read_bytes: None,
            })
            .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("line 1 and its truncation marker")
        );
    }

    #[tokio::test]
    async fn read_numbers_lines_and_respects_ranges() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let machine = test_machine(root.clone());

        let result = machine
            .read_for_test(Parameters(ReadArgs {
                path: "a.txt".to_owned(),
                offset: None,
                limit: None,
                numbered: None,

                max_read_bytes: None,
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        assert_eq!(
            serialized["content"][0]["text"].as_str().unwrap(),
            "1\talpha\n2\tbeta\n3\tgamma"
        );

        let result = machine
            .read_for_test(Parameters(ReadArgs {
                path: "a.txt".to_owned(),
                offset: Some(2),
                limit: Some(1),
                numbered: None,

                max_read_bytes: None,
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        assert_eq!(
            serialized["content"][0]["text"].as_str().unwrap(),
            "2\tbeta\n… truncated at line 2"
        );

        let result = machine
            .read_for_test(Parameters(ReadArgs {
                path: "a.txt".to_owned(),
                offset: Some(2),
                limit: None,
                numbered: Some(false),

                max_read_bytes: None,
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        // `numbered: false` returns the window byte for byte, newline included.
        assert_eq!(
            serialized["content"][0]["text"].as_str().unwrap(),
            "beta\ngamma\n"
        );

        // CRLF and a missing trailing newline survive the round trip.
        std::fs::write(root.join("crlf.txt"), "a\r\nb\r\n").unwrap();
        let result = machine
            .read_for_test(Parameters(ReadArgs {
                path: "crlf.txt".to_owned(),
                offset: None,
                limit: None,
                numbered: Some(false),

                max_read_bytes: None,
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        assert_eq!(
            serialized["content"][0]["text"].as_str().unwrap(),
            "a\r\nb\r\n"
        );
        // Numbered rendering strips the terminator so lines join cleanly.
        let result = machine
            .read_for_test(Parameters(ReadArgs {
                path: "crlf.txt".to_owned(),
                offset: None,
                limit: None,
                numbered: None,

                max_read_bytes: None,
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        assert_eq!(
            serialized["content"][0]["text"].as_str().unwrap(),
            "1\ta\n2\tb"
        );

        // Reading past the end of the file is an error, not an empty answer.
        let result = machine
            .read_for_test(Parameters(ReadArgs {
                path: "a.txt".to_owned(),
                offset: Some(50),
                limit: None,
                numbered: None,

                max_read_bytes: None,
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        assert_eq!(serialized["isError"], serde_json::json!(true));
        assert!(
            serialized["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("offset 50 is beyond end of file (3 lines)")
        );

        // The default window is 4096 lines and ends with exactly one marker.
        let total = DEFAULT_READ_LINES + 5;
        let big = (1..=total)
            .map(|line| format!("line{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(machine.config.access.base_dir.join("big.txt"), &big).unwrap();
        let result = machine
            .read_for_test(Parameters(ReadArgs {
                path: "big.txt".to_owned(),
                offset: None,
                limit: None,
                numbered: Some(false),

                max_read_bytes: None,
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        let text = serialized["content"][0]["text"].as_str().unwrap();
        assert!(text.ends_with(&format!(
            "line{DEFAULT_READ_LINES}\n\u{2026} truncated at line {DEFAULT_READ_LINES}"
        )));
        assert_eq!(text.matches("truncated at line").count(), 1);
    }

    #[tokio::test]
    async fn read_stops_at_the_byte_limit_with_whole_lines_and_a_marker() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let line = format!("{}\n", "x".repeat(200));
        std::fs::write(root.join("wide.txt"), line.repeat(10)).unwrap();
        let machine = crate::mcp::tools::test_support::test_machine_with(root, |config| {
            config.max_read_bytes = 1024
        });
        let result = machine
            .read_for_test(Parameters(ReadArgs {
                path: "wide.txt".to_owned(),
                offset: None,
                limit: None,
                numbered: Some(false),

                max_read_bytes: None,
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        let text = serialized["content"][0]["text"].as_str().unwrap();

        // The ceiling bounds the returned text, marker included: a fifth line
        // would push the marker past 1024 bytes, and a line is never cut to
        // make room for the marker.
        assert_eq!(text.matches('x').count(), 4 * 200, "{text}");
        assert!(text.ends_with("\n… truncated at line 4"), "{text}");
        assert!(text.len() <= 1024, "{} bytes", text.len());
        for line in text.lines().filter(|line| line.starts_with('x')) {
            assert_eq!(line.len(), 200, "a returned line was cut short");
        }
    }

    #[tokio::test]
    async fn read_skips_lines_longer_than_the_whole_ceiling() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut content = "x".repeat(2000);
        content.push_str("\nSECOND\n");
        std::fs::write(root.join("a.txt"), content).unwrap();
        let machine = test_machine_with(root, |config| config.max_read_bytes = 1024);

        // A window selected by `offset` must not depend on the size of what
        // precedes it.
        let result = machine
            .read_for_test(Parameters(ReadArgs {
                path: "a.txt".to_owned(),
                offset: Some(2),
                limit: Some(1),
                numbered: None,

                max_read_bytes: None,
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        assert_eq!(
            serialized["content"][0]["text"].as_str().unwrap(),
            "2\tSECOND"
        );
    }

    #[tokio::test]
    async fn read_rejects_first_nonexistent_line_after_eof() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = test_machine(root.clone());
        for (source, content, lines) in [
            ("trailing-lf.txt", "first\n", 1),
            ("trailing-crlf.txt", "first\r\nsecond\r\n", 2),
            ("unterminated.txt", "first", 1),
            ("empty-line.txt", "\n", 1),
        ] {
            std::fs::write(root.join(source), content).unwrap();
            let result = machine
                .read_for_test(Parameters(ReadArgs {
                    path: source.to_owned(),
                    offset: Some(lines + 1),
                    limit: Some(1),
                    numbered: Some(false),

                    max_read_bytes: None,
                }))
                .await;
            let result = serde_json::to_value(result).unwrap();
            assert_eq!(result["isError"], true, "{source}: {result}");
            assert!(
                result["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("({lines} lines)"))
            );
        }
        std::fs::write(root.join("empty.txt"), "").unwrap();
        let result = machine
            .read_for_test(Parameters(ReadArgs {
                path: "empty.txt".to_owned(),
                offset: Some(1),
                limit: Some(1),
                numbered: Some(false),

                max_read_bytes: None,
            }))
            .await;
        assert_eq!(
            serde_json::to_value(result).unwrap()["content"][0]["text"],
            ""
        );
    }

    #[tokio::test]
    async fn read_builds_a_page_of_short_lines_in_one_buffer() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        // 20 000 empty lines: a per-line allocation would dwarf the reply.
        std::fs::write(root.join("many.txt"), "\n".repeat(20_000)).unwrap();
        let machine = test_machine_with(root, |config| config.max_read_bytes = 4096);

        let result = machine
            .read_for_test(Parameters(ReadArgs {
                path: "many.txt".to_owned(),
                offset: None,
                limit: None,
                numbered: None,

                max_read_bytes: None,
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        let text = serialized["content"][0]["text"].as_str().unwrap();
        assert!(text.len() <= 4096, "the reply is bounded by the ceiling");
        let lines = text.lines().collect::<Vec<_>>();
        let (&marker, page) = lines.split_last().expect("a marker line");
        assert!(marker.starts_with("… truncated at line "), "{marker:?}");
        assert!(page.len() > 100, "many short lines fit in one buffer");
        for (index, line) in page.iter().enumerate() {
            // Whole rendered lines only: `N` followed by the empty body.
            assert_eq!(*line, format!("{}\t", index + 1), "a line was cut short");
        }
    }

    #[tokio::test]
    async fn read_reports_a_line_that_does_not_fit_after_numbering() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        // 1023 bytes plus the newline is the whole ceiling; numbering it does
        // not fit, and the answer must name the line instead of "line 0".
        std::fs::write(root.join("a.txt"), format!("{}\n", "x".repeat(1023))).unwrap();
        let machine = test_machine_with(root, |config| config.max_read_bytes = 1024);

        let result = machine
            .read_for_test(Parameters(ReadArgs {
                path: "a.txt".to_owned(),
                offset: None,
                limit: None,
                numbered: None,

                max_read_bytes: None,
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        assert_eq!(serialized["isError"], serde_json::json!(true));
        let error = serialized["content"][0]["text"].as_str().unwrap();
        assert!(
            error.contains("line 1 does not fit the 1024 byte read limit"),
            "{error}"
        );

        // Without the numbering prefix the same line fits.
        let result = machine
            .read_for_test(Parameters(ReadArgs {
                path: "a.txt".to_owned(),
                offset: None,
                limit: None,
                numbered: Some(false),

                max_read_bytes: None,
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        let text = serialized["content"][0]["text"].as_str().unwrap();
        assert_eq!(text.len(), 1024, "the window is returned byte for byte");
    }

    #[tokio::test]
    async fn read_decodes_only_the_returned_window() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.bin"), [0xff_u8, b'\n', b'o', b'k', b'\n']).unwrap();
        let machine = test_machine(root);

        let result = machine
            .read_for_test(Parameters(ReadArgs {
                path: "a.bin".to_owned(),
                offset: Some(2),
                limit: None,
                numbered: None,

                max_read_bytes: None,
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        assert_eq!(serialized["content"][0]["text"].as_str().unwrap(), "2\tok");
    }

    #[tokio::test]
    async fn escape_heavy_read_pages_fit_the_serialized_mcp_result_budget() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let line = format!("{}\n", "\"".repeat(2040));
        let content = line.repeat(4096);
        assert!(content.len() < crate::limits::OPENAI_TUNNEL_TRANSFER_CEILING_BYTES);
        std::fs::write(root.join("quotes.txt"), &content).unwrap();
        let machine = test_machine_with(root, |config| {
            config.max_read_bytes = crate::limits::OPENAI_TUNNEL_TRANSFER_CEILING_BYTES;
        });
        let text = machine
            .read_impl_with_result_budget(
                ReadArgs {
                    path: "quotes.txt".to_owned(),
                    offset: None,
                    limit: Some(4096),
                    numbered: Some(false),
                    max_read_bytes: Some(crate::limits::OPENAI_TUNNEL_TRANSFER_CEILING_BYTES),
                },
                Some(crate::limits::OPENAI_TUNNEL_TOOL_RESULT_BYTES),
            )
            .await
            .unwrap();
        assert!(
            text.len() < content.len(),
            "escape-heavy page should be shortened"
        );
        assert!(text.contains("… truncated at line"), "{text:?}");
        assert!(
            crate::mcp::serialized_text_tool_result_len(&text)
                <= crate::limits::OPENAI_TUNNEL_TOOL_RESULT_BYTES
        );

        let local = machine
            .read_impl(ReadArgs {
                path: "quotes.txt".to_owned(),
                offset: None,
                limit: Some(4096),
                numbered: Some(false),
                max_read_bytes: Some(crate::limits::OPENAI_TUNNEL_TRANSFER_CEILING_BYTES),
            })
            .await
            .unwrap();
        assert_eq!(
            local, content,
            "local transport must retain the full raw profile ceiling"
        );
    }

    #[tokio::test]
    async fn read_streams_a_window_without_loading_the_whole_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut content = String::from("first line\n");
        content.push_str(&"x".repeat(80 * 1024));
        content.push('\n');
        std::fs::write(root.join("large.txt"), content).unwrap();
        let machine = crate::mcp::tools::test_support::test_machine_with(root, |config| {
            config.max_read_bytes = 1024
        });

        let result = machine
            .read_for_test(Parameters(ReadArgs {
                path: "large.txt".to_owned(),
                offset: None,
                limit: Some(1),
                numbered: None,

                max_read_bytes: None,
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        assert_eq!(
            serialized["content"][0]["text"].as_str().unwrap(),
            "1\tfirst line\n… truncated at line 1"
        );
    }

    #[tokio::test]
    async fn read_zero_limit_never_renders_a_nonsense_marker() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "alpha\nbeta\n").unwrap();
        let machine = test_machine(root);

        let result = machine
            .read_for_test(Parameters(ReadArgs {
                path: "a.txt".to_owned(),
                offset: None,
                limit: Some(0),
                numbered: None,

                max_read_bytes: None,
            }))
            .await;
        let serialized = serde_json::to_value(&result).unwrap();
        let text = serialized["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, "1\talpha\n… truncated at line 1");
        assert!(!text.contains("line 0"));
    }
}
