//! Shared byte ceilings for untrusted external bodies and private state files.
//! Enforce while reading, before decoding or deserialization.

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body::Body as HttpBody;
use http_body_util::BodyExt as _;
use std::{
    fs,
    io::{self, Read as _},
    path::Path,
};

pub(crate) const PROFILE_CONFIG_MAX_BYTES: u64 = 4 * 1024 * 1024;
pub(crate) const RUNTIME_KEY_MAX_BYTES: u64 = 16 * 1024;

pub(crate) fn ensure_file_size(contents: &[u8], limit: u64, label: &'static str) -> Result<()> {
    if u64::try_from(contents.len()).map_or(true, |length| length > limit) {
        bail!("{label} exceeds {limit} bytes");
    }
    Ok(())
}

/// Open one private control-plane file without blocking on Unix special files.
/// Validate the opened descriptor rather than a raceable pathname.
pub(crate) fn open_regular_file(path: &Path, label: &'static str) -> Result<Option<fs::File>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot open {label}: {}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect {label}: {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{label} must be a regular file: {}", path.display());
    }
    Ok(Some(file))
}

/// Read a private control-plane UTF-8 file with the same streaming limit
/// discipline as HTTP bodies. A missing path is not an error; callers decide
/// whether absence is expected. Reject files grown mid-read.
pub(crate) fn text_file(path: &Path, limit: u64, label: &'static str) -> Result<Option<String>> {
    let Some(file) = open_regular_file(path, label)? else {
        return Ok(None);
    };
    if file.metadata()?.len() > limit {
        bail!(
            "{label} must be a regular file of at most {limit} bytes: {}",
            path.display()
        );
    }
    let mut contents = String::new();
    file.take(limit.saturating_add(1))
        .read_to_string(&mut contents)
        .with_context(|| format!("cannot read {label}: {}", path.display()))?;
    if contents.len() as u64 > limit {
        bail!("{label} exceeds {limit} bytes: {}", path.display());
    }
    Ok(Some(contents))
}

struct Buffer {
    bytes: Vec<u8>,
    limit: usize,
    label: &'static str,
}

impl Buffer {
    fn new(limit: usize, label: &'static str) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            label,
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<()> {
        if chunk.len() > self.limit.saturating_sub(self.bytes.len()) {
            bail!("{} exceeded {} bytes", self.label, self.limit);
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    fn finish(self) -> Bytes {
        Bytes::from(self.bytes)
    }
}

/// Bounded collection of an HTTP response received from another process.
pub(crate) async fn reqwest_response(
    mut response: reqwest::Response,
    limit: usize,
    label: &'static str,
) -> Result<Bytes> {
    let mut result = Buffer::new(limit, label);
    while let Some(chunk) = response.chunk().await? {
        result.push(&chunk)?;
    }
    Ok(result.finish())
}

/// The same byte ceiling for an in-process HTTP body (the OpenAI tunnel's
/// embedded MCP service).
pub(crate) async fn local_response<B>(
    mut body: B,
    limit: usize,
    label: &'static str,
) -> Result<Bytes>
where
    B: HttpBody<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    let mut result = Buffer::new(limit, label);
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|error| anyhow::anyhow!("failed reading {}: {error}", label))?;
        if let Some(data) = frame.data_ref() {
            result.push(data)?;
        }
    }
    Ok(result.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_file_reads_are_stream_bounded_and_explain_missing_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        assert_eq!(text_file(&path, 5, "state").unwrap(), None);
        fs::write(&path, "abcde").unwrap();
        assert_eq!(text_file(&path, 5, "state").unwrap().unwrap(), "abcde");
        assert!(text_file(&path, 4, "state").is_err());
        assert!(text_file(temp.path(), 5, "state").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unix_fifo_and_device_reads_are_rejected_without_blocking() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt as _, sync::mpsc, thread, time::Duration};

        let temp = tempfile::tempdir().unwrap();
        let fifo = temp.path().join("state.fifo");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo_c is a live, NUL-terminated path and the mode is valid.
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);

        let (sender, receiver) = mpsc::channel();
        let reader_path = fifo.clone();
        let reader = thread::spawn(move || {
            let _ = sender.send(text_file(&reader_path, 5, "state"));
        });
        let result = match receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Keep this regression test from hanging even if O_NONBLOCK is
                // accidentally removed: pair with the blocked FIFO reader.
                let _ = fs::OpenOptions::new().write(true).open(&fifo);
                let _ = reader.join();
                panic!("opening a FIFO for a bounded read blocked");
            }
            Err(error) => panic!("FIFO reader stopped unexpectedly: {error}"),
        };
        reader.join().unwrap();
        assert!(result.is_err());
        assert!(text_file(Path::new("/dev/null"), 5, "state").is_err());
    }

    #[test]
    fn checks_each_chunk_and_preserves_exact_limit() {
        let mut buffer = Buffer::new(5, "test response");
        buffer.push(b"ab").unwrap();
        buffer.push(b"cde").unwrap();
        assert!(
            buffer
                .push(b"f")
                .unwrap_err()
                .to_string()
                .contains("5 bytes")
        );
        assert_eq!(buffer.finish(), Bytes::from_static(b"abcde"));
    }

    #[tokio::test]
    async fn remote_response_stops_on_the_first_oversized_chunk() {
        use tokio::io::AsyncWriteExt as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut request).await;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabcdef",
                )
                .await
                .unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("http://{address}/");
        let response = client.get(url).send().await.unwrap();
        assert!(
            reqwest_response(response, 5, "poll envelope")
                .await
                .unwrap_err()
                .to_string()
                .contains("5 bytes")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn in_process_body_is_bounded() {
        let body = http_body_util::Full::new(Bytes::from(vec![b'x'; 100]));
        assert!(local_response(body, 4, "test body").await.is_err());
    }
}
