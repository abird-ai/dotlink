use std::{
    pin::Pin,
    task::{Context as TaskContext, Poll},
};

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use serde_json::Value;
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::sync::CancellationToken;

use crate::{logging::LogConfig, mcp::LocalMachine};

const MAX_LOGGED_REQUEST_PREFIX: usize = 64 * 1024;

struct RequestLoggingReader<R> {
    inner: R,
    log: LogConfig,
    pending: Vec<u8>,
    overflow: bool,
    in_line: bool,
    capture_line: bool,
}

impl<R> RequestLoggingReader<R> {
    fn new(inner: R, log: LogConfig) -> Self {
        Self {
            inner,
            log,
            pending: Vec::new(),
            overflow: false,
            in_line: false,
            capture_line: false,
        }
    }

    fn observe(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            let enabled = self.log.developer_enabled();
            if byte == b'\n' {
                if self.capture_line {
                    self.log_pending();
                }
                self.pending.clear();
                self.overflow = false;
                self.in_line = false;
                self.capture_line = false;
                continue;
            }

            if !self.in_line {
                self.in_line = true;
                self.capture_line = enabled;
            } else if self.capture_line && !enabled {
                // Verbosity was turned off mid-request. Drop the partial line;
                // if it is turned back on before newline we still wait for the
                // next complete JSON-RPC frame instead of splicing fragments.
                self.capture_line = false;
                self.pending.clear();
                self.overflow = false;
            }

            if !self.capture_line {
                continue;
            }

            if self.pending.len() < MAX_LOGGED_REQUEST_PREFIX {
                self.pending.push(byte);
            } else {
                self.overflow = true;
            }
        }
    }

    fn log_pending(&self) {
        let line = self
            .pending
            .strip_suffix(b"\r")
            .unwrap_or(self.pending.as_slice());
        if line.is_empty() {
            return;
        }

        let method = serde_json::from_slice::<Value>(line)
            .ok()
            .and_then(|value| {
                value
                    .get("method")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .or_else(|| extract_method_prefix(line));

        if let Some(method) = method {
            let suffix = if self.overflow {
                " (large request)"
            } else {
                ""
            };
            let _ = self.log.request("stdio", &format!("{method}{suffix}"));
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for RequestLoggingReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);

        if let Poll::Ready(Ok(())) = &poll {
            let after = buf.filled().len();
            if after > before {
                let bytes = buf.filled()[before..after].to_vec();
                self.observe(&bytes);
            }
        }

        poll
    }
}

fn extract_method_prefix(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let key = "\"method\"";
    let start = text.find(key)? + key.len();
    let rest = text[start..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

pub async fn run(machine: LocalMachine, cancellation: CancellationToken) -> Result<()> {
    let (stdin, stdout) = rmcp::transport::stdio();
    let reader = RequestLoggingReader::new(stdin, machine.log().clone());
    let service = machine
        .serve_with_ct((reader, stdout), cancellation)
        .await
        .context("failed to initialize stdio MCP server")?;

    service
        .waiting()
        .await
        .context("stdio MCP server task failed")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_prefix_extraction_avoids_payload_logging() {
        let request =
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"content":"secret"}}"#;
        assert_eq!(
            extract_method_prefix(request).as_deref(),
            Some("tools/call")
        );
    }
}
