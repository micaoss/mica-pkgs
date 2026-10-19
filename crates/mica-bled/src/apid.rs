//! mica-apid on its local socket: the routes of protocol section 7.1, and
//! the PIN. apid authorizes the caller by its uid; nothing is sent to prove it.

use std::path::PathBuf;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use serde_json::Value;

/// How long one call to apid may take; a Wi-Fi scan is the slowest.
const TIMEOUT: Duration = Duration::from_secs(30);

/// apid's answer: its status and its JSON body, if any.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub status: u16,
    pub body: Option<Value>,
}

/// apid could not be asked, or gave no usable answer.
#[derive(Debug)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mica-apid: {}", self.0)
    }
}

impl std::error::Error for Unavailable {}

#[async_trait::async_trait]
pub trait Apid: Send + Sync {
    /// One request; `path` is apid's, without the `/api` prefix.
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Reply, Unavailable>;

    /// The device's PIN, `bluetooth.pin`.
    async fn pin(&self) -> Result<String, Unavailable> {
        let reply = self.request("GET", "/v1/bluetooth", None).await?;
        match (
            reply.status,
            reply.body.as_ref().and_then(|b| b["pin"].as_str()),
        ) {
            (200, Some(pin)) => Ok(pin.to_string()),
            (status, _) => Err(Unavailable(format!(
                "GET /v1/bluetooth answered {status} without a pin"
            ))),
        }
    }

    /// Write `bluetooth.pin`.
    async fn set_pin(&self, pin: &str) -> Result<Reply, Unavailable> {
        self.request(
            "PUT",
            "/v1/settings/bluetooth.pin",
            Some(&Value::String(pin.to_string())),
        )
        .await
    }
}

/// apid over HTTP/1.1 on its local Unix socket.
pub struct HttpApid {
    pub socket: PathBuf,
}

impl HttpApid {
    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        let stream = tokio::net::UnixStream::connect(&self.socket).await?;
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(conn);
        let body = match body {
            Some(value) => Bytes::from(serde_json::to_vec(value)?),
            None => Bytes::new(),
        };
        let request = hyper::Request::builder()
            .method(method)
            .uri(format!("/api{path}"))
            .header(hyper::header::HOST, "localhost")
            .header(hyper::header::ACCEPT, "application/json")
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(Full::new(body))?;
        let response = sender.send_request(request).await?;
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await?.to_bytes();
        let body = if bytes.is_empty() {
            None
        } else {
            serde_json::from_slice(&bytes).ok()
        };
        Ok(Reply { status, body })
    }
}

#[async_trait::async_trait]
impl Apid for HttpApid {
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Reply, Unavailable> {
        match tokio::time::timeout(TIMEOUT, self.call(method, path, body)).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(err)) => Err(Unavailable(err.to_string())),
            Err(_) => Err(Unavailable(format!("{method} {path} timed out"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One connection: returns the request head, answers with `response`.
    async fn serve_once(listener: tokio::net::UnixListener, response: &'static str) -> String {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = vec![0; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        stream.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    #[tokio::test]
    async fn the_pin_is_read_over_the_local_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("apid.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(serve_once(
            listener,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 16\r\n\r\n{\"pin\":\"424242\"}",
        ));
        let apid = HttpApid { socket };
        assert_eq!(apid.pin().await.unwrap(), "424242");
        let head = server.await.unwrap();
        assert!(
            head.starts_with("GET /api/v1/bluetooth HTTP/1.1\r\n"),
            "{head}"
        );
        assert!(
            !head.to_ascii_lowercase().contains("authorization"),
            "{head}"
        );
    }

    #[tokio::test]
    async fn the_pin_write_is_a_json_string_and_the_reply_passes_through() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("apid.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(serve_once(
            listener,
            "HTTP/1.1 422 Unprocessable Entity\r\ncontent-length: 15\r\n\r\n{\"code\":\"bad\"}\n",
        ));
        let reply = HttpApid { socket }.set_pin("123456").await.unwrap();
        assert_eq!(reply.status, 422);
        assert_eq!(reply.body, Some(serde_json::json!({"code": "bad"})));
        let head = server.await.unwrap();
        assert!(
            head.starts_with("PUT /api/v1/settings/bluetooth.pin HTTP/1.1\r\n"),
            "{head}"
        );
        assert!(head.ends_with("\"123456\""), "{head}");
    }

    #[tokio::test]
    async fn no_socket_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let apid = HttpApid {
            socket: dir.path().join("absent.sock"),
        };
        assert!(apid.pin().await.is_err());
    }
}
