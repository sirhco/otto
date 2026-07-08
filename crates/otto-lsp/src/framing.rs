use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

pub fn encode(body: &[u8]) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

pub struct FrameReader<R> {
    inner: R,
}

impl<R: AsyncBufRead + Unpin> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Reads one Content-Length framed message body. Returns None on clean EOF.
    pub async fn next_frame(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            let n = self.inner.read_line(&mut line).await?;
            if n == 0 {
                return Ok(None); // EOF before any header
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break; // end of headers
            }
            if let Some((k, v)) = trimmed.split_once(':')
                && k.eq_ignore_ascii_case("Content-Length")
            {
                content_length = v.trim().parse().ok();
            }
        }
        let len = content_length.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "missing Content-Length")
        })?;
        let mut body = vec![0u8; len];
        self.inner.read_exact(&mut body).await?;
        Ok(Some(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn roundtrip_two_frames() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&encode(br#"{"a":1}"#));
        buf.extend_from_slice(&encode(br#"{"b":2}"#));
        let mut r = FrameReader::new(BufReader::new(&buf[..]));
        assert_eq!(r.next_frame().await.unwrap().unwrap(), br#"{"a":1}"#);
        assert_eq!(r.next_frame().await.unwrap().unwrap(), br#"{"b":2}"#);
        assert!(r.next_frame().await.unwrap().is_none());
    }
}
