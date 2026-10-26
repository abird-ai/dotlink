//! Byte ceilings for external HTTP bodies. Enforce during streaming, before
//! deserialization or decoding; headers and post-hoc string slicing are not caps.

use anyhow::{Result, bail};
use bytes::Bytes;
use http_body::Body as HttpBody;
use http_body_util::BodyExt as _;

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
