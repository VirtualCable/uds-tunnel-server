// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
//
// Regression tests for the pre-auth broker request timeout.
//
// History: `connection::connect` called `broker::start_connection(...)` BEFORE
// the `max_sessions_per_remote` check and BEFORE `SessionManager::add_session`
// (which is where the global `max_sessions` cap lives), and `start_connection`
// issued `client.post(...).send().await?` on a reqwest `Client` built with no
// `.timeout()` (reqwest default is `None`). An unauthenticated peer could pin
// one server task + one held TCP socket + one in-flight outbound broker HTTP
// request per connection, with no in-code bound, for as long as the broker
// took to answer (indefinitely if it hung).
//
// Fix: `start_connection` now applies `crate::broker::START_CONNECTION_TIMEOUT`
// (5 s) to the ticket request, bounding the pre-auth pinned window per
// connection. These tests pin that bound: with a broker endpoint that accepts
// and never answers, every unauthenticated Open must be released by the
// request timeout — not pinned forever.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

use shared::protocol::consts::{HANDSHAKE_V2_SIGNATURE, TICKET_LENGTH};

use crate::config;
use crate::session::SessionManager;

/// The broker timeout plus generous slack for task scheduling under load.
/// Pre-fix, a connection was pinned for the broker's (indefinite) reply time;
/// post-fix every one must be released inside this window.
const fn add_secs(d: Duration, secs: u64) -> Duration {
    Duration::new(d.as_secs() + secs, d.subsec_nanos())
}
const RELEASE_DEADLINE: Duration = add_secs(crate::broker::START_CONNECTION_TIMEOUT, 5);

fn open_handshake() -> Vec<u8> {
    let mut hs = Vec::with_capacity(HANDSHAKE_V2_SIGNATURE.len() + 1 + TICKET_LENGTH);
    hs.extend_from_slice(HANDSHAKE_V2_SIGNATURE);
    hs.push(1u8); // HandshakeCommand::Open
    // Any 48 alphanumeric bytes pass `Ticket::validate()`; no broker
    // credential is needed to reach `start_connection`.
    hs.extend_from_slice(&[b'A'; TICKET_LENGTH]);
    hs
}

/// A broker endpoint that accepts connections and never writes a response,
/// so every request to it hangs until the caller's timeout fires.
/// Returns the accept counter and the URL.
async fn start_hanging_broker() -> (Arc<AtomicUsize>, String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let acc = accepted.clone();
    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((sock, _)) => {
                    acc.fetch_add(1, Ordering::SeqCst);
                    // Hold the socket open forever, never answering.
                    tokio::spawn(async move {
                        let _sock = sock;
                        std::future::pending::<()>().await;
                    });
                }
                Err(e) => {
                    eprintln!("[regression] hanging-broker accept error: {e:?}");
                    break;
                }
            }
        }
    });
    (accepted, format!("http://{}/", addr), task)
}

fn set_config(ticket_api_url: String, max_sessions: usize, max_sessions_per_remote: usize) {
    let cfg = config::get();
    let mut c = cfg.write().unwrap();
    c.ticket_api_url = ticket_api_url;
    c.broker_auth_token = "test_token".to_string();
    c.dangerous_disable_ssl_verify = Some(false);
    c.use_proxy_protocol = Some(false);
    c.max_sessions = Some(max_sessions);
    c.max_sessions_per_remote = Some(max_sessions_per_remote);
}

/// Drives one unauthenticated `Open` handshake through the real
/// `handle_connection` entry point over an in-memory duplex stream.
///
/// `keep_open`: keep the attacker's socket half open. When `false` the half
/// is dropped right after the handshake is written, which models an attacker
/// that sends the handshake and immediately closes its connection.
fn spawn_open_connection(keep_open: bool) -> tokio::task::JoinHandle<()> {
    let (mut client, server) = tokio::io::duplex(4096);
    tokio::spawn(async move {
        let _ = client.write_all(&open_handshake()).await;
        let ip: std::net::SocketAddr = "127.0.0.1:12345".parse().unwrap();
        if keep_open {
            let _held = client; // lives until the broker call resolves
            let _ = crate::connection::handle_connection(server, tokio::io::sink(), ip).await;
        } else {
            drop(client);
            let _ = crate::connection::handle_connection(server, tokio::io::sink(), ip).await;
        }
    })
}

fn restore_caps() {
    let cfg = config::get();
    let mut c = cfg.write().unwrap();
    c.max_sessions = None;
    c.max_sessions_per_remote = None;
}

/// Unauthenticated Opens against a hung broker are released by the
/// start_connection request timeout within a bounded window, instead of
/// pinning task + socket + HTTP request forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(config, manager)]
async fn unauthenticated_open_is_released_by_the_broker_timeout() {
    const N: usize = 32;

    let (accepted, url, broker_task) = start_hanging_broker().await;
    // Caps as tight as they can be set: one session, one per source IP.
    set_config(url, 1, 1);

    // Baseline of the global manager count. Other tests in this binary may
    // leave sessions registered when they finish (they are serialized with
    // this one under the `manager` key, so the baseline is stable across the
    // whole test); what this test pins is that *its own* connections add none.
    let sessions_before = SessionManager::get_instance().count();

    let handles: Vec<_> = (0..N).map(|_| spawn_open_connection(true)).collect();

    tokio::time::sleep(RELEASE_DEADLINE).await;

    let finished = handles.iter().filter(|h| h.is_finished()).count();
    let broker_inflight = accepted.load(Ordering::SeqCst);
    let sessions = SessionManager::get_instance().count();
    println!(
        "[regression] after {RELEASE_DEADLINE:?}: released={finished}/{N} broker_requests_issued={broker_inflight} max_sessions_cap=1 sessions_registered={} (baseline {sessions_before})",
        sessions
    );

    // FIXED BEHAVIOUR PIN: pre-fix, `finished` was 0 forever (the pin).
    // Post-fix every connection must be done — the request timed out and the
    // error propagated, ending the handshake attempt.
    assert_eq!(
        finished, N,
        "every unauthenticated Open must be released by the broker request timeout"
    );
    assert!(
        broker_inflight >= N,
        "each connection still reaches the broker call before timing out (got {broker_inflight})"
    );
    assert_eq!(
        sessions, sessions_before,
        "a timed-out broker call never reaches add_session"
    );

    broker_task.abort();
    restore_caps();
}

/// The release does not depend on the attacker keeping its socket open: the
/// timeout bounds the pinned resources even when the peer vanishes after the
/// handshake.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(config, manager)]
async fn release_happens_even_if_the_attacker_closes_its_connection() {
    const N: usize = 16;

    let (accepted, url, broker_task) = start_hanging_broker().await;
    set_config(url, 1, 1);

    let handles: Vec<_> = (0..N).map(|_| spawn_open_connection(false)).collect();

    tokio::time::sleep(RELEASE_DEADLINE).await;

    let finished = handles.iter().filter(|h| h.is_finished()).count();
    let broker_inflight = accepted.load(Ordering::SeqCst);
    println!(
        "[regression] attacker sockets closed after handshake: released={finished}/{N} broker_requests_issued={broker_inflight}"
    );
    assert_eq!(
        finished, N,
        "the request timeout must release the task even with the attacker socket gone"
    );
    assert!(broker_inflight >= N);

    broker_task.abort();
    restore_caps();
}

/// Negative control: with a broker that answers immediately the same
/// handshake completes (and is rejected by the broker) promptly, showing the
/// timeout does not affect the healthy path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(config, manager)]
async fn responsive_broker_does_not_delay_the_handshake() {
    let mut server = mockito::Server::new_async().await;
    let _m = server.mock("POST", "/").with_status(500).create();
    set_config(server.url() + "/", 1, 1);

    let handle = spawn_open_connection(true);
    let completed = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(
        completed.is_ok(),
        "with a responsive broker the connection must complete promptly, the \
         5s start_connection timeout must not fire on a healthy broker"
    );

    restore_caps();
}

/// Quantification, run explicitly:
/// `PIN_N=512 cargo test -p tunnel-server --bin tunnel-server \
///   tests_broker_timeout::scaling_measurement -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(config, manager)]
#[ignore = "measurement only, run with --ignored"]
async fn scaling_measurement() {
    let n: usize = std::env::var("PIN_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);

    let (accepted, url, broker_task) = start_hanging_broker().await;
    set_config(url, 1, 1);

    let fds_before = std::fs::read_dir("/proc/self/fd")
        .map(|d| d.count())
        .unwrap_or(0);

    let handles: Vec<_> = (0..n).map(|_| spawn_open_connection(true)).collect();

    // Give the swarm time to reach the broker call and poll until every one
    // of them has landed there before judging the release window.
    let mut waited = Duration::ZERO;
    while waited < Duration::from_secs(30) && accepted.load(Ordering::SeqCst) < n {
        tokio::time::sleep(Duration::from_millis(250)).await;
        waited += Duration::from_millis(250);
    }
    println!(
        "[measurement] N={n}: reached broker call after {waited:?} (accepted={})",
        accepted.load(Ordering::SeqCst)
    );

    // Post-fix: all of them must be released by the timeout within the
    // bounded window (this is what pre-fix could never happen).
    tokio::time::sleep(RELEASE_DEADLINE).await;
    let finished = handles.iter().filter(|h| h.is_finished()).count();
    let broker_inflight = accepted.load(Ordering::SeqCst);
    let fds_after = std::fs::read_dir("/proc/self/fd")
        .map(|d| d.count())
        .unwrap_or(0);
    println!(
        "[measurement] N={n}: released={finished}/{n} broker_requests_issued={broker_inflight} sessions={} fds_before={fds_before} fds_after={fds_after} (delta={})",
        SessionManager::get_instance().count(),
        fds_after.saturating_sub(fds_before)
    );

    assert_eq!(
        finished, n,
        "the timeout must release the whole swarm, not pin it"
    );
    assert!(broker_inflight >= n);

    broker_task.abort();
    restore_caps();
}
