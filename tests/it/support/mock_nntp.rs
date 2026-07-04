//! In-process mock NNTP server for integration tests.
//!
//! Plain TCP (no TLS), scripted greeting, optional AUTHINFO USER/PASS, a
//! per-message-id article store served as dot-stuffed multiline BODY
//! responses, configurable missing articles (430), mid-body disconnects and
//! response delays. Connection counters support pool/reaper assertions.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use usenet_streaming_server::db::providers::Provider;

struct ServerState {
    articles: Mutex<HashMap<String, Arc<Vec<u8>>>>,
    drop_mid_body: Mutex<HashSet<String>>,
    delay: Mutex<Option<Duration>>,
    auth: Option<(String, String)>,
    open: AtomicUsize,
    total: AtomicUsize,
}

pub struct MockNntp {
    addr: SocketAddr,
    state: Arc<ServerState>,
    kick: watch::Sender<u64>,
}

impl MockNntp {
    /// Start on an ephemeral port. `auth` = Some((user, pass)) makes
    /// AUTHINFO mandatory before STAT/BODY.
    pub async fn start(auth: Option<(&str, &str)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().expect("mock addr");
        let state = Arc::new(ServerState {
            articles: Mutex::new(HashMap::new()),
            drop_mid_body: Mutex::new(HashSet::new()),
            delay: Mutex::new(None),
            auth: auth.map(|(u, p)| (u.to_string(), p.to_string())),
            open: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
        });
        let (kick, _) = watch::channel(0u64);

        let accept_state = state.clone();
        let kick_tx = kick.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                accept_state.open.fetch_add(1, Ordering::SeqCst);
                accept_state.total.fetch_add(1, Ordering::SeqCst);
                let conn_state = accept_state.clone();
                let kick_rx = kick_tx.subscribe();
                tokio::spawn(async move {
                    handle_connection(stream, conn_state.clone(), kick_rx).await;
                    conn_state.open.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });

        Self { addr, state, kick }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// A `Provider` row pointing at this mock (credentials filled in when the
    /// server requires auth).
    pub fn provider(&self, name: &str, priority: i64, max_connections: i64) -> Provider {
        Provider {
            id: 0,
            name: name.to_string(),
            host: "127.0.0.1".to_string(),
            port: self.addr.port(),
            use_tls: false,
            username: self.state.auth.as_ref().map(|(u, _)| u.clone()),
            password: self.state.auth.as_ref().map(|(_, p)| p.clone()),
            max_connections,
            priority,
            enabled: true,
        }
    }

    /// Store an article body (raw yEnc text, CRLF lines). `message_id` is
    /// stored without angle brackets.
    pub fn add_article(&self, message_id: &str, body: Vec<u8>) {
        self.state.articles.lock().unwrap().insert(
            message_id.trim_matches(['<', '>']).to_string(),
            Arc::new(body),
        );
    }

    /// Make an article 430 again.
    pub fn remove_article(&self, message_id: &str) {
        self.state
            .articles
            .lock()
            .unwrap()
            .remove(message_id.trim_matches(['<', '>']));
    }

    /// Serve only half of this article's body, then drop the connection.
    pub fn drop_mid_body(&self, message_id: &str) {
        self.state
            .drop_mid_body
            .lock()
            .unwrap()
            .insert(message_id.trim_matches(['<', '>']).to_string());
    }

    /// Inject a delay before every response.
    pub fn set_delay(&self, delay: Option<Duration>) {
        *self.state.delay.lock().unwrap() = delay;
    }

    /// Forcibly close all currently open connections.
    pub fn disconnect_all(&self) {
        self.kick.send_modify(|v| *v += 1);
    }

    pub fn open_connections(&self) -> usize {
        self.state.open.load(Ordering::SeqCst)
    }

    pub fn total_connections(&self) -> usize {
        self.state.total.load(Ordering::SeqCst)
    }

    /// Poll until `open_connections() == n` (bounded wait, panics on timeout).
    pub async fn wait_for_open(&self, n: usize) {
        for _ in 0..200 {
            if self.open_connections() == n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "mock server never reached {n} open connections (now {})",
            self.open_connections()
        );
    }
}

/// NNTP dot-stuffing: prefix lines starting with '.' by another '.'; ensure
/// the body ends with CRLF so the terminator sits on its own line.
fn dot_stuff(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 16);
    let mut at_line_start = true;
    for &b in body {
        if at_line_start && b == b'.' {
            out.push(b'.');
        }
        out.push(b);
        at_line_start = b == b'\n';
    }
    if !out.ends_with(b"\n") {
        out.extend_from_slice(b"\r\n");
    }
    out
}

async fn handle_connection(
    stream: TcpStream,
    state: Arc<ServerState>,
    mut kick: watch::Receiver<u64>,
) {
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    if writer.write_all(b"200 mock NNTP ready\r\n").await.is_err() {
        return;
    }

    let mut authed = state.auth.is_none();
    let mut pending_user: Option<String> = None;
    let mut line = String::new();

    loop {
        line.clear();
        let n = tokio::select! {
            result = reader.read_line(&mut line) => match result {
                Ok(n) => n,
                Err(_) => return,
            },
            _ = kick.changed() => return,
        };
        if n == 0 {
            return;
        }
        let delay = *state.delay.lock().unwrap();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }

        let command = line.trim_end();
        let mut parts = command.splitn(3, ' ');
        let verb = parts.next().unwrap_or("").to_ascii_uppercase();

        let ok = match verb.as_str() {
            "AUTHINFO" => {
                let sub = parts.next().unwrap_or("").to_ascii_uppercase();
                let value = parts.next().unwrap_or("").to_string();
                match (&state.auth, sub.as_str()) {
                    (Some(_), "USER") => {
                        pending_user = Some(value);
                        writer.write_all(b"381 password required\r\n").await
                    }
                    (Some((user, pass)), "PASS") => {
                        if pending_user.as_deref() == Some(user.as_str()) && value == *pass {
                            authed = true;
                            writer.write_all(b"281 authentication accepted\r\n").await
                        } else {
                            writer.write_all(b"481 authentication failed\r\n").await
                        }
                    }
                    _ => {
                        writer
                            .write_all(b"281 no authentication required\r\n")
                            .await
                    }
                }
            }
            "STAT" => {
                if !authed {
                    writer.write_all(b"480 authentication required\r\n").await
                } else {
                    let id = strip_brackets(parts.next().unwrap_or(""));
                    if state.articles.lock().unwrap().contains_key(&id) {
                        writer
                            .write_all(format!("223 0 <{id}>\r\n").as_bytes())
                            .await
                    } else {
                        writer.write_all(b"430 no such article\r\n").await
                    }
                }
            }
            "BODY" => {
                if !authed {
                    writer.write_all(b"480 authentication required\r\n").await
                } else {
                    let id = strip_brackets(parts.next().unwrap_or(""));
                    let article = state.articles.lock().unwrap().get(&id).cloned();
                    match article {
                        None => writer.write_all(b"430 no such article\r\n").await,
                        Some(body) => {
                            let stuffed = dot_stuff(&body);
                            if state.drop_mid_body.lock().unwrap().contains(&id) {
                                let _ = writer
                                    .write_all(format!("222 0 <{id}>\r\n").as_bytes())
                                    .await;
                                let _ = writer.write_all(&stuffed[..stuffed.len() / 2]).await;
                                let _ = writer.flush().await;
                                return; // simulate a server-side crash mid-body
                            }
                            let mut response = format!("222 0 <{id}>\r\n").into_bytes();
                            response.extend_from_slice(&stuffed);
                            response.extend_from_slice(b".\r\n");
                            writer.write_all(&response).await
                        }
                    }
                }
            }
            "DATE" => writer.write_all(b"111 20260704120000\r\n").await,
            "QUIT" => {
                let _ = writer.write_all(b"205 goodbye\r\n").await;
                return;
            }
            _ => writer.write_all(b"500 unknown command\r\n").await,
        };
        if ok.is_err() {
            return;
        }
    }
}

fn strip_brackets(token: &str) -> String {
    token.trim().trim_matches(['<', '>']).to_string()
}
