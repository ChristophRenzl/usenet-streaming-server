//! A single NNTP connection: TCP (optionally TLS via rustls/webpki-roots),
//! line-based command framing, AUTHINFO, and the handful of commands the
//! streamer needs (STAT/BODY/DATE/QUIT).

use std::{sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::TlsConnector;

use crate::db::providers::Provider;

/// Errors from the NNTP layer. `ArticleNotFound` is a normal condition (430,
/// 423 or a missing article on one provider); everything else indicates a
/// connection- or protocol-level failure.
#[derive(Debug, thiserror::Error)]
pub enum NntpError {
    #[error("article not found")]
    ArticleNotFound,

    #[error("authentication failed: {0}")]
    AuthFailed(String),

    #[error("invalid message-id: {0:?}")]
    InvalidMessageId(String),

    #[error("unexpected NNTP response: {0}")]
    UnexpectedResponse(String),

    #[error("NNTP {0} timed out")]
    Timeout(&'static str),

    #[error("TLS error: {0}")]
    Tls(String),

    #[error("no NNTP providers configured")]
    NoProviders,

    #[error("NNTP I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Connect/read/write deadlines for a single connection.
#[derive(Debug, Clone, Copy)]
pub struct NntpTimeouts {
    pub connect: Duration,
    pub read: Duration,
    pub write: Duration,
}

impl Default for NntpTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            read: Duration::from_secs(30),
            write: Duration::from_secs(30),
        }
    }
}

trait AsyncStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> AsyncStream for T {}

pub struct NntpConnection {
    stream: BufReader<Box<dyn AsyncStream>>,
    timeouts: NntpTimeouts,
    poisoned: bool,
    /// True while a command is in flight. Command futures are not
    /// cancellation-safe (a cancelled `BODY` leaves its response in the
    /// socket), so a connection dropped mid-command must not be reused.
    busy: bool,
}

impl std::fmt::Debug for NntpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NntpConnection")
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            Arc::new(
                rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            )
        })
        .clone()
}

/// Wrap a bare message-id in angle brackets unless it already has them.
fn angle(message_id: &str) -> Result<String, NntpError> {
    if message_id.is_empty()
        || message_id
            .bytes()
            .any(|b| b == b'\r' || b == b'\n' || b == b' ' || b.is_ascii_control())
    {
        return Err(NntpError::InvalidMessageId(message_id.to_string()));
    }
    if message_id.starts_with('<') {
        Ok(message_id.to_string())
    } else {
        Ok(format!("<{message_id}>"))
    }
}

fn response_code(line: &str) -> Option<u16> {
    line.get(..3)?.parse().ok()
}

impl NntpConnection {
    /// Dial the provider, read the greeting and authenticate when credentials
    /// are configured.
    pub async fn connect(provider: &Provider, timeouts: NntpTimeouts) -> Result<Self, NntpError> {
        let tcp = timeout(
            timeouts.connect,
            TcpStream::connect((provider.host.as_str(), provider.port)),
        )
        .await
        .map_err(|_| NntpError::Timeout("connect"))??;
        tcp.set_nodelay(true).ok();

        let stream: Box<dyn AsyncStream> = if provider.use_tls {
            let server_name = rustls::pki_types::ServerName::try_from(provider.host.clone())
                .map_err(|e| NntpError::Tls(format!("invalid server name: {e}")))?;
            let connector = TlsConnector::from(tls_config());
            let tls = timeout(timeouts.connect, connector.connect(server_name, tcp))
                .await
                .map_err(|_| NntpError::Timeout("TLS handshake"))??;
            Box::new(tls)
        } else {
            Box::new(tcp)
        };

        let mut conn = Self {
            stream: BufReader::new(stream),
            timeouts,
            poisoned: false,
            busy: false,
        };

        let greeting = conn.read_line().await?;
        match response_code(&greeting) {
            Some(200) | Some(201) => {}
            _ => {
                return Err(NntpError::UnexpectedResponse(format!(
                    "greeting: {greeting}"
                )))
            }
        }

        if let Some(username) = provider.username.as_deref().filter(|u| !u.is_empty()) {
            conn.authenticate(username, provider.password.as_deref().unwrap_or(""))
                .await?;
        }
        Ok(conn)
    }

    async fn authenticate(&mut self, username: &str, password: &str) -> Result<(), NntpError> {
        let resp = self.command(&format!("AUTHINFO USER {username}")).await?;
        match response_code(&resp) {
            Some(281) => return Ok(()),
            Some(381) => {}
            _ => return Err(NntpError::AuthFailed(resp)),
        }
        let resp = self.command(&format!("AUTHINFO PASS {password}")).await?;
        match response_code(&resp) {
            Some(281) => Ok(()),
            _ => Err(NntpError::AuthFailed(resp)),
        }
    }

    /// True once a protocol or I/O error occurred, or when a command future
    /// was cancelled mid-flight (leaving the stream desynchronized); the
    /// pool discards such connections instead of reusing them.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned || self.busy
    }

    pub fn mark_poisoned(&mut self) {
        self.poisoned = true;
    }

    async fn send_line(&mut self, line: &str) -> Result<(), NntpError> {
        let mut buf = Vec::with_capacity(line.len() + 2);
        buf.extend_from_slice(line.as_bytes());
        buf.extend_from_slice(b"\r\n");
        let write = async {
            self.stream.get_mut().write_all(&buf).await?;
            self.stream.get_mut().flush().await
        };
        match timeout(self.timeouts.write, write).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                self.poisoned = true;
                Err(e.into())
            }
            Err(_) => {
                self.poisoned = true;
                Err(NntpError::Timeout("write"))
            }
        }
    }

    async fn read_line(&mut self) -> Result<String, NntpError> {
        let mut line = Vec::new();
        let n = match timeout(self.timeouts.read, self.stream.read_until(b'\n', &mut line)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                self.poisoned = true;
                return Err(e.into());
            }
            Err(_) => {
                self.poisoned = true;
                return Err(NntpError::Timeout("read"));
            }
        };
        if n == 0 {
            self.poisoned = true;
            return Err(NntpError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "server closed connection",
            )));
        }
        while line.last().is_some_and(|&b| b == b'\n' || b == b'\r') {
            line.pop();
        }
        Ok(String::from_utf8_lossy(&line).into_owned())
    }

    /// Send one command and return the single-line status response.
    async fn command(&mut self, line: &str) -> Result<String, NntpError> {
        self.send_line(line).await?;
        self.read_line().await
    }

    /// `STAT <id>`: does the article exist on this server?
    pub async fn stat(&mut self, message_id: &str) -> Result<bool, NntpError> {
        self.busy = true;
        let resp = self
            .command(&format!("STAT {}", angle(message_id)?))
            .await?;
        match response_code(&resp) {
            Some(223) => {
                self.busy = false;
                Ok(true)
            }
            Some(430) | Some(423) => {
                self.busy = false;
                Ok(false)
            }
            _ => {
                self.poisoned = true;
                Err(NntpError::UnexpectedResponse(format!("STAT: {resp}")))
            }
        }
    }

    /// `BODY <id>`: the raw (yEnc-encoded) article body with dot-stuffing
    /// removed and the terminating `.` line stripped. CRLF line breaks are
    /// preserved for the yEnc decoder.
    pub async fn body(&mut self, message_id: &str) -> Result<Bytes, NntpError> {
        self.busy = true;
        let resp = self
            .command(&format!("BODY {}", angle(message_id)?))
            .await?;
        match response_code(&resp) {
            Some(222) => {
                let body = self.read_multiline().await?;
                self.busy = false;
                Ok(body)
            }
            Some(430) | Some(423) => {
                self.busy = false;
                Err(NntpError::ArticleNotFound)
            }
            _ => {
                self.poisoned = true;
                Err(NntpError::UnexpectedResponse(format!("BODY: {resp}")))
            }
        }
    }

    async fn read_multiline(&mut self) -> Result<Bytes, NntpError> {
        let mut out = BytesMut::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            let n =
                match timeout(self.timeouts.read, self.stream.read_until(b'\n', &mut line)).await {
                    Ok(Ok(n)) => n,
                    Ok(Err(e)) => {
                        self.poisoned = true;
                        return Err(e.into());
                    }
                    Err(_) => {
                        self.poisoned = true;
                        return Err(NntpError::Timeout("read"));
                    }
                };
            if n == 0 {
                self.poisoned = true;
                return Err(NntpError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed mid-article",
                )));
            }
            if line == b".\r\n" || line == b".\n" {
                return Ok(out.freeze());
            }
            if line.starts_with(b".") {
                out.extend_from_slice(&line[1..]);
            } else {
                out.extend_from_slice(&line);
            }
        }
    }

    /// `DATE`: server time as `yyyymmddhhmmss`.
    pub async fn date(&mut self) -> Result<String, NntpError> {
        self.busy = true;
        let resp = self.command("DATE").await?;
        match response_code(&resp) {
            Some(111) => {
                self.busy = false;
                Ok(resp[3..].trim().to_string())
            }
            _ => {
                self.poisoned = true;
                Err(NntpError::UnexpectedResponse(format!("DATE: {resp}")))
            }
        }
    }

    /// Best-effort `QUIT`.
    pub async fn quit(&mut self) {
        if self.send_line("QUIT").await.is_ok() {
            let _ = self.read_line().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn angle_adds_brackets_only_when_missing() {
        assert_eq!(angle("a@b").unwrap(), "<a@b>");
        assert_eq!(angle("<a@b>").unwrap(), "<a@b>");
    }

    #[test]
    fn angle_rejects_dangerous_ids() {
        assert!(angle("").is_err());
        assert!(angle("a b").is_err());
        assert!(angle("a\r\nQUIT").is_err());
    }

    #[test]
    fn response_code_parses_prefix() {
        assert_eq!(response_code("223 0 <x> article exists"), Some(223));
        assert_eq!(response_code("x"), None);
    }
}
