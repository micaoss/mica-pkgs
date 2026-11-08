//! The GCE metadata server: the whole document, then each change to it.
//!
//! One request reads `/computeMetadata/v1/?recursive=true&alt=json`; with the
//! ETag of the last answer it becomes a long poll (`wait_for_change`) that the
//! server answers when the document changes or after `timeout_sec` seconds.
//! Every request carries `Metadata-Flavor: Google`, and an answer without that
//! header is not from the metadata server and is refused.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use serde_json::Value;

/// How long the server may hold a long poll.
pub const POLL_SECONDS: u64 = 60;

/// Slack on top of the long poll before the request is abandoned.
const SLACK: Duration = Duration::from_secs(10);

/// The metadata server at one address.
#[derive(Debug, Clone)]
pub struct Client {
    pub address: SocketAddr,
    pub poll_seconds: u64,
}

/// One answer: the document and its ETag.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub document: Value,
    pub etag: Option<String>,
}

impl Client {
    pub fn new(address: SocketAddr) -> Self {
        Self {
            address,
            poll_seconds: POLL_SECONDS,
        }
    }

    fn path(&self, etag: Option<&str>) -> String {
        let mut path = "/computeMetadata/v1/?recursive=true&alt=json".to_string();
        if let Some(etag) = etag {
            path.push_str(&format!(
                "&wait_for_change=true&last_etag={etag}&timeout_sec={}",
                self.poll_seconds
            ));
        }
        path
    }

    /// The document now (`etag` None), or once it differs from `etag`.
    pub async fn get(&self, etag: Option<&str>) -> Result<Answer> {
        if etag.is_some_and(|e| !e.chars().all(|c| c.is_ascii_alphanumeric())) {
            bail!("the metadata ETag is not alphanumeric");
        }
        let limit = Duration::from_secs(self.poll_seconds) + SLACK;
        tokio::time::timeout(limit, self.request(&self.path(etag)))
            .await
            .with_context(|| format!("no answer from {} in {limit:?}", self.address))?
    }

    async fn request(&self, path: &str) -> Result<Answer> {
        let stream = tokio::net::TcpStream::connect(self.address)
            .await
            .with_context(|| format!("connecting to {}", self.address))?;
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(conn);
        let request = hyper::Request::builder()
            .uri(path)
            .header(hyper::header::HOST, "metadata.google.internal")
            .header("Metadata-Flavor", "Google")
            .body(Empty::<Bytes>::new())?;
        let response = sender.send_request(request).await?;
        let status = response.status();
        if response
            .headers()
            .get("Metadata-Flavor")
            .is_none_or(|v| v != "Google")
        {
            bail!("the answer carries no `Metadata-Flavor: Google`");
        }
        let etag = response
            .headers()
            .get(hyper::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = response.into_body().collect().await?.to_bytes();
        if !status.is_success() {
            bail!(
                "the metadata server answered {status}: {}",
                String::from_utf8_lossy(&body).trim()
            );
        }
        let document = serde_json::from_slice(&body).context("the metadata is not JSON")?;
        Ok(Answer { document, etag })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_long_poll_names_the_etag() {
        let client = Client::new("127.0.0.1:80".parse().unwrap());
        assert_eq!(
            client.path(None),
            "/computeMetadata/v1/?recursive=true&alt=json"
        );
        assert_eq!(
            client.path(Some("ab12")),
            "/computeMetadata/v1/?recursive=true&alt=json&wait_for_change=true&last_etag=ab12&timeout_sec=60"
        );
    }

    #[tokio::test]
    async fn an_odd_etag_is_never_sent() {
        let client = Client::new("127.0.0.1:9".parse().unwrap());
        let err = client.get(Some("a&b")).await.unwrap_err().to_string();
        assert!(err.contains("not alphanumeric"), "{err}");
    }
}
