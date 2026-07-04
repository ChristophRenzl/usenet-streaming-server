//! NNTP connection + pool integration tests against the mock server.

use std::time::Duration;

use usenet_streaming_server::nntp::{
    test_provider, NntpConnection, NntpError, NntpPool, NntpTimeouts, PoolOptions,
};

use crate::support::MockNntp;

fn fast_timeouts() -> NntpTimeouts {
    NntpTimeouts {
        connect: Duration::from_secs(2),
        read: Duration::from_secs(2),
        write: Duration::from_secs(2),
    }
}

fn fast_options() -> PoolOptions {
    PoolOptions {
        timeouts: fast_timeouts(),
        ..PoolOptions::default()
    }
}

#[tokio::test]
async fn auth_flow_and_date() {
    let server = MockNntp::start(Some(("alice", "s3cret"))).await;
    let provider = server.provider("primary", 0, 2);

    let mut conn = NntpConnection::connect(&provider, fast_timeouts())
        .await
        .expect("connect + auth");
    let date = conn.date().await.expect("DATE");
    assert!(date.starts_with("2026"), "unexpected DATE payload: {date}");
    conn.quit().await;
}

#[tokio::test]
async fn wrong_password_is_auth_failure() {
    let server = MockNntp::start(Some(("alice", "s3cret"))).await;
    let mut provider = server.provider("primary", 0, 2);
    provider.password = Some("wrong".into());

    match NntpConnection::connect(&provider, fast_timeouts()).await {
        Err(NntpError::AuthFailed(resp)) => assert!(resp.starts_with("481"), "resp: {resp}"),
        other => panic!("expected AuthFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn commands_require_auth_on_protected_server() {
    let server = MockNntp::start(Some(("alice", "s3cret"))).await;
    server.add_article("a@mock", b"data\r\n".to_vec());
    let mut provider = server.provider("primary", 0, 2);
    provider.username = None; // skip AUTHINFO entirely
    provider.password = None;

    let mut conn = NntpConnection::connect(&provider, fast_timeouts())
        .await
        .expect("connect without auth");
    // Server answers 480; the client maps that to an unexpected response.
    assert!(matches!(
        conn.stat("a@mock").await,
        Err(NntpError::UnexpectedResponse(_))
    ));
}

#[tokio::test]
async fn body_round_trips_dot_stuffed_lines() {
    let server = MockNntp::start(None).await;
    let body: &[u8] = b".leading dot\r\n..two dots\r\nplain line\r\n...three\r\n";
    server.add_article("dots@mock", body.to_vec());

    let provider = server.provider("primary", 0, 2);
    let mut conn = NntpConnection::connect(&provider, fast_timeouts())
        .await
        .expect("connect");
    let got = conn.body("dots@mock").await.expect("BODY");
    assert_eq!(&got[..], body, "dot-unstuffing must restore the raw body");
}

#[tokio::test]
async fn stat_hit_and_miss() {
    let server = MockNntp::start(None).await;
    server.add_article("exists@mock", b"x\r\n".to_vec());
    let provider = server.provider("primary", 0, 2);

    let mut conn = NntpConnection::connect(&provider, fast_timeouts())
        .await
        .expect("connect");
    assert!(conn.stat("exists@mock").await.expect("STAT hit"));
    assert!(!conn.stat("missing@mock").await.expect("STAT miss"));

    // Same through the pool.
    let pool = NntpPool::with_options(vec![server.provider("p", 0, 2)], fast_options());
    assert!(pool.stat_any("exists@mock").await.expect("stat_any hit"));
    assert!(!pool.stat_any("missing@mock").await.expect("stat_any miss"));
}

#[tokio::test]
async fn missing_body_is_article_not_found() {
    let server = MockNntp::start(None).await;
    let pool = NntpPool::with_options(vec![server.provider("p", 0, 2)], fast_options());
    assert!(matches!(
        pool.fetch_body("nope@mock").await,
        Err(NntpError::ArticleNotFound)
    ));
}

#[tokio::test]
async fn pool_enforces_max_connections() {
    let server = MockNntp::start(None).await;
    let pool = NntpPool::with_options(vec![server.provider("p", 0, 2)], fast_options());

    let first = pool.checkout().await.expect("first checkout");
    let second = pool.checkout().await.expect("second checkout");

    // Third checkout must block while both permits are held...
    let blocked = tokio::time::timeout(Duration::from_millis(150), pool.checkout()).await;
    assert!(blocked.is_err(), "third checkout should wait for a permit");

    // ...and proceed as soon as one is returned.
    drop(first);
    let third = tokio::time::timeout(Duration::from_secs(2), pool.checkout())
        .await
        .expect("checkout after release should not time out")
        .expect("checkout");
    drop(third);
    drop(second);
}

#[tokio::test]
async fn provider_fallback_on_missing_article() {
    let primary = MockNntp::start(None).await;
    let backup = MockNntp::start(None).await;
    backup.add_article("only-on-backup@mock", b"backup body\r\n".to_vec());

    let pool = NntpPool::with_options(
        vec![
            primary.provider("primary", 0, 2),
            backup.provider("backup", 1, 2),
        ],
        fast_options(),
    );

    let body = pool
        .fetch_body("only-on-backup@mock")
        .await
        .expect("fallback fetch");
    assert_eq!(&body[..], b"backup body\r\n");
    assert!(
        primary.total_connections() >= 1,
        "primary must be tried first"
    );
    assert!(pool.stat_any("only-on-backup@mock").await.expect("stat"));
}

#[tokio::test]
async fn provider_fallback_on_connect_failure() {
    // A dead provider (bound then dropped listener) falls through to a live one.
    let dead_port = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let live = MockNntp::start(None).await;
    live.add_article("a@mock", b"hello\r\n".to_vec());

    let mut dead = live.provider("dead", 0, 2);
    dead.port = dead_port;
    let pool = NntpPool::with_options(vec![dead, live.provider("live", 1, 2)], fast_options());

    let body = pool.fetch_body("a@mock").await.expect("fetch via live");
    assert_eq!(&body[..], b"hello\r\n");
}

#[tokio::test]
async fn reconnects_after_server_side_disconnect() {
    let server = MockNntp::start(None).await;
    server.add_article("a@mock", b"first\r\n".to_vec());
    let pool = NntpPool::with_options(vec![server.provider("p", 0, 2)], fast_options());

    let body = pool.fetch_body("a@mock").await.expect("first fetch");
    assert_eq!(&body[..], b"first\r\n");
    assert_eq!(server.total_connections(), 1);

    // Kill the pooled idle connection server-side.
    server.disconnect_all();
    server.wait_for_open(0).await;

    // The stale idle connection fails and is transparently replaced.
    let body = pool
        .fetch_body("a@mock")
        .await
        .expect("fetch after disconnect");
    assert_eq!(&body[..], b"first\r\n");
    assert_eq!(server.total_connections(), 2, "one reconnect expected");
}

#[tokio::test]
async fn mid_body_disconnect_falls_back_to_next_provider() {
    let flaky = MockNntp::start(None).await;
    let stable = MockNntp::start(None).await;
    let body: Vec<u8> = b"0123456789abcdef\r\n".repeat(64);
    flaky.add_article("cut@mock", body.clone());
    flaky.drop_mid_body("cut@mock");
    stable.add_article("cut@mock", body.clone());

    let pool = NntpPool::with_options(
        vec![
            flaky.provider("flaky", 0, 2),
            stable.provider("stable", 1, 2),
        ],
        fast_options(),
    );
    let got = pool.fetch_body("cut@mock").await.expect("fallback fetch");
    assert_eq!(got[..], body[..]);

    // With only the flaky provider the failure surfaces.
    let pool = NntpPool::with_options(vec![flaky.provider("flaky", 0, 2)], fast_options());
    assert!(pool.fetch_body("cut@mock").await.is_err());
}

#[tokio::test]
async fn reaper_closes_idle_but_not_in_use_connections() {
    let server = MockNntp::start(None).await;
    let options = PoolOptions {
        timeouts: fast_timeouts(),
        idle_ttl: Duration::from_millis(200),
        reap_interval: Duration::from_millis(50),
    };
    let pool = NntpPool::with_options(vec![server.provider("p", 0, 4)], options);

    let mut held = pool.checkout().await.expect("held checkout");
    let idle = pool.checkout().await.expect("idle checkout");
    drop(idle); // returns to the idle list
    assert_eq!(server.open_connections(), 2);

    // Give the reaper several cycles past the TTL.
    tokio::time::sleep(Duration::from_millis(600)).await;
    server.wait_for_open(1).await; // idle one reaped, held one alive

    // The held connection is still fully functional.
    let date = held.date().await.expect("held connection still works");
    assert!(date.starts_with("2026"));
}

#[tokio::test]
async fn test_provider_reports_latency_or_error() {
    let server = MockNntp::start(Some(("u", "p"))).await;
    let latency = test_provider(&server.provider("p", 0, 1))
        .await
        .expect("test ok");
    assert!(latency < Duration::from_secs(2));

    let dead_port = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let mut dead = server.provider("dead", 0, 1);
    dead.port = dead_port;
    let err = test_provider(&dead).await.expect_err("dead provider");
    assert!(!err.is_empty());
}

#[tokio::test]
async fn pool_reload_swaps_providers() {
    let old = MockNntp::start(None).await;
    let new = MockNntp::start(None).await;
    new.add_article("fresh@mock", b"new data\r\n".to_vec());

    let pool = NntpPool::with_options(vec![old.provider("old", 0, 2)], fast_options());
    assert!(matches!(
        pool.fetch_body("fresh@mock").await,
        Err(NntpError::ArticleNotFound)
    ));

    pool.reload(vec![new.provider("new", 0, 2)]);
    let body = pool.fetch_body("fresh@mock").await.expect("after reload");
    assert_eq!(&body[..], b"new data\r\n");
}

#[tokio::test]
async fn delayed_responses_hit_read_timeout() {
    let server = MockNntp::start(None).await;
    server.add_article("slow@mock", b"x\r\n".to_vec());

    let timeouts = NntpTimeouts {
        connect: Duration::from_secs(2),
        read: Duration::from_millis(100),
        write: Duration::from_secs(2),
    };
    // The greeting is sent immediately, so connect succeeds...
    let mut conn = NntpConnection::connect(&server.provider("p", 0, 1), timeouts)
        .await
        .expect("connect");

    // ...but a delayed command response trips the read timeout.
    server.set_delay(Some(Duration::from_millis(500)));
    match conn.stat("slow@mock").await {
        Err(NntpError::Timeout(op)) => assert_eq!(op, "read"),
        other => panic!("expected read timeout, got {other:?}"),
    }
    assert!(conn.is_poisoned(), "timed-out connection must be poisoned");
}

#[tokio::test]
async fn cancelled_fetch_does_not_desync_the_pooled_connection() {
    // Regression test: dropping an in-flight BODY future (e.g. a media
    // client disconnecting or ffmpeg being killed mid-read) must not return
    // a desynchronized connection to the idle list — the next command would
    // otherwise read the *previous* command's response.
    let server = MockNntp::start(None).await;
    server.add_article("first@mock", b"payload-of-first-article\r\n".to_vec());
    server.add_article("second@mock", b"payload-of-second-article\r\n".to_vec());

    // One connection, so a dirty idle connection would definitely be reused.
    let pool = NntpPool::with_options(vec![server.provider("p", 0, 1)], fast_options());

    // Warm the connection cleanly first.
    let body = pool.fetch_body("first@mock").await.expect("warm fetch");
    assert_eq!(&body[..], b"payload-of-first-article\r\n");

    // Cancel a fetch mid-command: the delayed response arrives after the
    // future is dropped and would linger in the socket buffer.
    server.set_delay(Some(Duration::from_millis(300)));
    let cancelled =
        tokio::time::timeout(Duration::from_millis(50), pool.fetch_body("first@mock")).await;
    assert!(cancelled.is_err(), "fetch must still be in flight");
    server.set_delay(None);

    // The next fetch must return the *right* article, not first@mock's
    // stale response.
    let body = pool.fetch_body("second@mock").await.expect("clean fetch");
    assert_eq!(&body[..], b"payload-of-second-article\r\n");
}
