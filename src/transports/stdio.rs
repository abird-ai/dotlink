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
        let mut start = 0;
        for end in memchr::memchr_iter(b'\n', bytes) {
            self.observe_fragment(&bytes[start..end]);
            if self.capture_line {
                self.log_pending();
            }
            self.pending.clear();
            self.overflow = false;
            self.in_line = false;
            self.capture_line = false;
            start = end + 1;
        }
        self.observe_fragment(&bytes[start..]);
    }

    /// Borrow each contiguous line fragment; only developer logging needs a
    /// bounded copy of its content. Check live verbosity once per fragment,
    /// not once per byte of potentially multi-MiB MCP requests.
    fn observe_fragment(&mut self, fragment: &[u8]) {
        if fragment.is_empty() {
            return;
        }
        if !self.in_line {
            self.in_line = true;
            self.capture_line = self.log.developer_enabled();
        } else if self.capture_line && !self.log.developer_enabled() {
            // Never resume logging a partial request if verbosity toggles.
            self.capture_line = false;
            self.pending.clear();
            self.overflow = false;
        }
        if self.capture_line {
            let count = fragment
                .len()
                .min(MAX_LOGGED_REQUEST_PREFIX - self.pending.len());
            self.pending.extend_from_slice(&fragment[..count]);
            self.overflow |= count < fragment.len();
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
                self.observe(&buf.filled()[before..after]);
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
        .map(|value| crate::logging::jsonrpc_request_label(&value, false))
        .unwrap_or_else(|_| "unknown JSON-RPC method".to_owned())
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
    fn quiet_stdio_inspection_is_zero_capture_and_tracks_frame_boundaries() {
        let mut reader = RequestLoggingReader::new(tokio::io::empty(), LogConfig::default());
        reader.observe(b"request-one\nrequest-two");
        assert!(reader.in_line);
        assert!(!reader.capture_line);
        assert!(reader.pending.is_empty());
        reader.observe(b"\r\n");
        assert!(!reader.in_line);
        assert!(reader.pending.is_empty());
    }

    #[test]
    fn stdio_inspection_caps_large_lines_and_drops_mid_line_verbosity_changes() {
        let log = LogConfig::default();
        log.cycle_verbosity();
        log.cycle_verbosity();
        let mut reader = RequestLoggingReader::new(tokio::io::empty(), log.clone());
        reader.observe(&vec![b'x'; MAX_LOGGED_REQUEST_PREFIX + 17]);
        assert_eq!(reader.pending.len(), MAX_LOGGED_REQUEST_PREFIX);
        assert!(reader.overflow);
        log.cycle_verbosity(); // disable REQ partway through this line
        reader.observe(b"trailing-data");
        assert!(!reader.capture_line);
        assert!(!reader.overflow);
        assert!(reader.pending.is_empty());
        log.cycle_verbosity();
        log.cycle_verbosity();
        reader.observe(b"still-same-line");
        assert!(reader.pending.is_empty(), "must not splice a partial line");
        reader.observe(b"\nnext");
        assert_eq!(reader.pending, b"next");
        assert!(reader.capture_line);
    }

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
