//! Multi-provider NNTP connection pool.
//!
//! Providers are tried in ascending `priority` order; an article that is
//! missing (430) on one provider falls through to the next. Per provider a
//! semaphore enforces `max_connections` and idle connections are kept for
//! reuse until a background reaper closes those unused for longer than the
//! idle TTL.

use std::{
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use bytes::Bytes;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::conn::{NntpConnection, NntpError, NntpTimeouts};
use crate::db::providers::Provider;

#[derive(Debug, Clone, Copy)]
pub struct PoolOptions {
    pub timeouts: NntpTimeouts,
    /// Idle connections older than this are closed by the reaper.
    pub idle_ttl: Duration,
    /// How often the reaper scans for stale idle connections.
    pub reap_interval: Duration,
}

impl Default for PoolOptions {
    fn default() -> Self {
        Self {
            timeouts: NntpTimeouts::default(),
            idle_ttl: Duration::from_secs(60),
            reap_interval: Duration::from_secs(15),
        }
    }
}

struct IdleConn {
    conn: NntpConnection,
    since: Instant,
}

struct ProviderSlot {
    provider: Provider,
    semaphore: Arc<Semaphore>,
    idle: Mutex<Vec<IdleConn>>,
}

struct PoolInner {
    slots: Mutex<Arc<Vec<Arc<ProviderSlot>>>>,
    options: PoolOptions,
}

impl PoolInner {
    fn slots(&self) -> Arc<Vec<Arc<ProviderSlot>>> {
        self.slots.lock().expect("pool lock").clone()
    }
}

fn build_slots(providers: Vec<Provider>) -> Arc<Vec<Arc<ProviderSlot>>> {
    let mut providers: Vec<Provider> = providers.into_iter().filter(|p| p.enabled).collect();
    providers.sort_by_key(|p| p.priority);
    Arc::new(
        providers
            .into_iter()
            .map(|provider| {
                let max = usize::try_from(provider.max_connections.max(1)).unwrap_or(1);
                Arc::new(ProviderSlot {
                    provider,
                    semaphore: Arc::new(Semaphore::new(max)),
                    idle: Mutex::new(Vec::new()),
                })
            })
            .collect(),
    )
}

/// Shared, reload-able NNTP connection pool. Cheap to clone.
#[derive(Clone)]
pub struct NntpPool {
    inner: Arc<PoolInner>,
}

impl NntpPool {
    /// Build a pool from the enabled providers. Must be called from within a
    /// tokio runtime (the idle-connection reaper is spawned here).
    pub fn new(providers: Vec<Provider>) -> Self {
        Self::with_options(providers, PoolOptions::default())
    }

    pub fn with_options(providers: Vec<Provider>, options: PoolOptions) -> Self {
        let inner = Arc::new(PoolInner {
            slots: Mutex::new(build_slots(providers)),
            options,
        });
        spawn_reaper(&inner);
        Self { inner }
    }

    /// Replace the provider configuration (e.g. after a settings change).
    /// Existing checked-out connections keep working against the old slots
    /// and are dropped once returned.
    pub fn reload(&self, providers: Vec<Provider>) {
        *self.inner.slots.lock().expect("pool lock") = build_slots(providers);
    }

    /// Check out a connection from the highest-priority provider that is
    /// reachable. Waits when the provider's `max_connections` are all in use.
    pub async fn checkout(&self) -> Result<PooledConn, NntpError> {
        let slots = self.inner.slots();
        let mut last_err = None;
        for slot in slots.iter() {
            match checkout_from(slot, &self.inner.options, None).await {
                Ok(conn) => return Ok(conn),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or(NntpError::NoProviders))
    }

    /// `STAT` with provider fallback: `Ok(true)` as soon as one provider has
    /// the article, `Ok(false)` when every reachable provider reports it
    /// missing, `Err` when no provider could be asked.
    pub async fn stat_any(&self, message_id: &str) -> Result<bool, NntpError> {
        let slots = self.inner.slots();
        let mut answered = false;
        let mut last_err = None;
        for slot in slots.iter() {
            match run_on_slot(slot, &self.inner.options, Command::Stat(message_id)).await {
                Ok(CommandOutput::Stat(true)) => return Ok(true),
                Ok(_) => answered = true,
                Err(e) => last_err = Some(e),
            }
        }
        if answered {
            Ok(false)
        } else {
            Err(last_err.unwrap_or(NntpError::NoProviders))
        }
    }

    /// `BODY` with provider fallback. Returns `ArticleNotFound` only after
    /// every reachable provider reported the article missing.
    pub async fn fetch_body(&self, message_id: &str) -> Result<Bytes, NntpError> {
        let slots = self.inner.slots();
        let mut not_found = false;
        let mut last_err = None;
        for slot in slots.iter() {
            match run_on_slot(slot, &self.inner.options, Command::Body(message_id)).await {
                Ok(CommandOutput::Body(body)) => return Ok(body),
                Ok(_) => {}
                Err(NntpError::ArticleNotFound) => not_found = true,
                Err(e) => last_err = Some(e),
            }
        }
        if not_found {
            Err(NntpError::ArticleNotFound)
        } else {
            Err(last_err.unwrap_or(NntpError::NoProviders))
        }
    }

    /// Non-blocking variant of [`fetch_body`](Self::fetch_body) for
    /// readahead: providers whose connection permits are all in use are
    /// skipped, so prefetching never starves demand reads. `Ok(None)` means
    /// "not fetched" (contended, missing or failed) and is not an error.
    pub async fn try_fetch_body(&self, message_id: &str) -> Option<Bytes> {
        let slots = self.inner.slots();
        for slot in slots.iter() {
            let Ok(permit) = slot.semaphore.clone().try_acquire_owned() else {
                continue;
            };
            match checkout_from(slot, &self.inner.options, Some(permit)).await {
                Ok(mut guard) => match guard.body(message_id).await {
                    Ok(body) => return Some(body),
                    Err(_) => continue,
                },
                Err(_) => continue,
            }
        }
        None
    }
}

#[derive(Clone, Copy)]
enum Command<'a> {
    Stat(&'a str),
    Body(&'a str),
}

enum CommandOutput {
    Stat(bool),
    Body(Bytes),
}

/// Run one command on a slot, transparently retrying with a fresh connection
/// when a previously-idle (possibly stale) connection fails.
async fn run_on_slot(
    slot: &Arc<ProviderSlot>,
    options: &PoolOptions,
    command: Command<'_>,
) -> Result<CommandOutput, NntpError> {
    let mut stale_retries = 0u32;
    loop {
        let mut guard = checkout_from(slot, options, None).await?;
        let from_idle = guard.from_idle;
        let result = match command {
            Command::Stat(id) => guard.stat(id).await.map(CommandOutput::Stat),
            Command::Body(id) => guard.body(id).await.map(CommandOutput::Body),
        };
        match result {
            Ok(v) => return Ok(v),
            Err(NntpError::ArticleNotFound) => return Err(NntpError::ArticleNotFound),
            Err(e) => {
                guard.mark_poisoned();
                stale_retries += 1;
                // An idle connection may have been closed server-side while
                // pooled; retry with a fresh one. Fresh-connection failures
                // are real errors.
                if from_idle && stale_retries <= 3 {
                    continue;
                }
                return Err(e);
            }
        }
    }
}

async fn checkout_from(
    slot: &Arc<ProviderSlot>,
    options: &PoolOptions,
    permit: Option<OwnedSemaphorePermit>,
) -> Result<PooledConn, NntpError> {
    let permit = match permit {
        Some(p) => p,
        None => slot
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| NntpError::NoProviders)?,
    };
    let idle = slot.idle.lock().expect("pool lock").pop();
    let (conn, from_idle) = match idle {
        Some(idle) => (idle.conn, true),
        None => (
            NntpConnection::connect(&slot.provider, options.timeouts).await?,
            false,
        ),
    };
    Ok(PooledConn {
        conn: Some(conn),
        slot: slot.clone(),
        from_idle,
        _permit: permit,
    })
}

/// RAII guard for a pooled connection: healthy connections return to the
/// provider's idle list on drop, poisoned ones are closed.
pub struct PooledConn {
    conn: Option<NntpConnection>,
    slot: Arc<ProviderSlot>,
    from_idle: bool,
    _permit: OwnedSemaphorePermit,
}

impl PooledConn {
    /// Name of the provider this connection belongs to.
    pub fn provider_name(&self) -> &str {
        &self.slot.provider.name
    }
}

impl Deref for PooledConn {
    type Target = NntpConnection;
    fn deref(&self) -> &Self::Target {
        self.conn.as_ref().expect("connection taken")
    }
}

impl DerefMut for PooledConn {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.conn.as_mut().expect("connection taken")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            if !conn.is_poisoned() {
                self.slot.idle.lock().expect("pool lock").push(IdleConn {
                    conn,
                    since: Instant::now(),
                });
            }
        }
    }
}

fn spawn_reaper(inner: &Arc<PoolInner>) {
    let weak: Weak<PoolInner> = Arc::downgrade(inner);
    let interval = inner.options.reap_interval;
    let ttl = inner.options.idle_ttl;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let Some(inner) = weak.upgrade() else { break };
            for slot in inner.slots().iter() {
                slot.idle
                    .lock()
                    .expect("pool lock")
                    .retain(|idle| idle.since.elapsed() < ttl);
            }
        }
    });
}

/// Dial a provider, authenticate and issue `DATE`; used by the
/// `POST /api/v1/settings/providers/{id}/test` endpoint.
pub async fn test_provider(provider: &Provider) -> Result<Duration, String> {
    let start = Instant::now();
    let mut conn = NntpConnection::connect(provider, NntpTimeouts::default())
        .await
        .map_err(|e| e.to_string())?;
    conn.date().await.map_err(|e| e.to_string())?;
    let latency = start.elapsed();
    conn.quit().await;
    Ok(latency)
}
