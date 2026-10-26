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

        let _ = self
            .log
            .request("stdio", &request_label(line, self.overflow));
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

/// Log only complete, printable, top-level JSON-RPC method names.
fn request_label(line: &[u8], overflow: bool) -> String {
    if overflow {
        return "large JSON-RPC request".to_owned();
    }
    serde_json::from_slice::<Value>(line)
        .ok()
        .and_then(|value| {
            value
                .get("method")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|method| {
            method.len() <= 96
                && method.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.')
                })
        })
        .unwrap_or_else(|| "unknown JSON-RPC method".to_owned())
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
    fn request_logging_rejects_nested_and_untrusted_method_fields() {
        let nested = br#"{"jsonrpc":"2.0","params":{"method":"fake-secret"}}"#;
        assert_eq!(request_label(nested, false), "unknown JSON-RPC method");
        assert_eq!(request_label(nested, true), "large JSON-RPC request");
        assert_eq!(
            request_label(
                br#"{"method":"tools/call","params":{"method":"fake-secret"}}"#,
                false
            ),
            "tools/call"
        );
        assert_eq!(
            request_label(br#"{"method":"api-token:secret/value"}"#, false),
            "unknown JSON-RPC method"
        );
        assert_eq!(
            request_label(br#"{"method":"tools/call\\nESCAPE"}"#, false),
            "unknown JSON-RPC method"
        );
        assert_eq!(
            request_label(br#"{"method":null,"params":{"method":"SECRET"}}"#, false),
            "unknown JSON-RPC method"
        );
    }
}
