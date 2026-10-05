//! Soak: whole-server teardown / resource-leak invariants under
//! hostile-but-authenticated client behaviour.
//!
//! Self-contained (does not reuse the private helpers in `connection::tests`)
//! so it can live outside the files the other regression suites occupy.
//!
//! Adversary: a client that owns a valid ticket (so it can complete the Open
//! handshake) and then misbehaves at the protocol level right before or after
//! tearing the connection down. For every behaviour the invariant is the same:
//! the `SessionManager` must return to its pre-iteration size — a session that
//! survives the client is a slot leak and, repeated, a `max_sessions` DoS.
#![allow(unused_imports)]

use std::sync::Arc;
use std::time::Duration;

use mockito::{Matcher, Server};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use shared::{
    crypt::{
        Crypt,
        tunnel::derive_tunnel_material,
        types::{PacketBuffer, SharedSecret},
    },
    protocol::{
        Command,
        consts::{HANDSHAKE_V2_SIGNATURE, TICKET_LENGTH, TUNNEL_AUTH_HEADER},
        handshake::HandshakeCommand,
        ticket::Ticket,
    },
};

use crate::{config, connection::handle_connection, session::SessionManager};

const TICKET_ID: &str = "c6s9FAa5fhb854BVMckqUBJ4hOXg2iE5i1FYPCuktks4eNZD";
const KNOWN_SECRET_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Pre-encrypted broker ticket response for `TICKET_ID` (deterministic
/// `cfg(test)` KEM keypair). One remote, notify = "B"*48.
const BROKER_RESPONSE: &str = r#"{
    "algorithm": "AES-256-GCM",
    "ciphertext": "kZSEYJN/z3zZTkmEZBsCwIVhG5MmtxTRZijJ/PGsX27TGv3qv7086X9PWXN3HFjyiZ22LbZD2fDJP7GlxtwjWQkqh8vx0c/E16xMFHDQuEItEJVZFUD3qcFHHlVuhukV7N1IDGer6YC6cxvE+J1z9ei6+97hEkp6S9g9SJmj+YbaCR63qdiKFpXDcs863ZFnFdUy3lWvpC42hD16xwsYcIOePXVknFlqGQ05eKK8NFH13T5l5890UUn5hcefGO6fBwQxU5Z09BYXZ3TFxpNCoFOoCE36b8SCZIRqggI0nN5zBsSeyv+BnQIVlerA5Pmt68pSfutGreYttS30n7ViYaLuBSKUBeS7NhZ/giyZF9A56StDOa2HH5/31Jja8cyFTKLi4XIcWPFCt7cMu4ADD3hifh3OSXDzs9QUmJmXWGasrIArYnhHfQxBPH7KQ9TihjywthRG5orJX9guJlYFdgHDtlYSyY0PTzHZzrTZXBCTBi/jo8T1EpHB+vGua/C1nssbeDPUqzNnCgkIhTXr7AmOaXMy3xZ8cfCsL+mMzbMlQUfhkK22S/7G4gi5t0/24iN/qN1xK2qo/rpPPwZ1W/+3NFgQ1saO/yiy8BIgfd1NDj6+d4fRrghETerH4gIrh64onmAu3YSGyDG+Tmo7DMIWjzwDEYxb30AGWe+0+FCDvU/l8JnNDTqdlZqimJmBj0zZ3UR4TORm+bGt4ODNL8DYIqLIvTYXsDGiWzwsLm+v2mGgij4OerDWuvaNn3suGoKsaY1nJwHtVRXXy6ViNIOwnlh18rUdtsZ9B2pVvQs/uQTR+b8YJOMKTGEFEO3d5XluLgFuwZ7QRtEGKCy7flkGVEzWygjeULtFydHviS8khcOkpWRRjpe3ID6h9leUIG1wdYO7lVvOVf82X611Ex5/g/RNl6ZHySup3oFIcTwdka5ypbN77nQg90Ti/+DLb3sLeEYy6vZW8BBOV9gRYWiHDrzxTeEOS7irZ2f+bzB6P3ff7lROEVHdAojPuZjXN3i9SXNdcMqcRUq4AGiIIomB8Dg0P4zH7ns9qEhgAZb117s18ugi8dJBIRcb0SeOPaHCjDyezS2gv6RVksxIhpfdscEtTLlyjjxwMJxMJq0C42FZFYIm1AwYXJ1NAAX4bg8wMuA3Q8qUIOpgoq9i9LQitzeGIBZj336MizvgVLJUEKrWsXv77p2fYUh6Hc/8J5JMjp5ifM7X2SmEBl0coBpaHMLrTw9pdPEWJbJWn4k6tpbDFlmJtaTviF2bToJ398vwlsyUT/eO9tKIZuMM7GoxYlbHtH+8ttTaajbpfufuIreMW3WdJjjnPBJJ79kodHbMR+UPxwuIEuUIqWFNGQN4/6TnbSRNUsMpso84IbiIPFbQ8goAlZds9cf65cRvpkHitLDTFqh6tEmOUwdM2vlZqcjMk17dvaRgZDAbfPw=",
    "data": "9F8HAK6YkJ2kIcX+IhsPhvV3NISJC05z1W84zsK+apovP7tD7tpIkK5RLYNCKGDuJgNzQqMU1Wdj/B+YTLOcpJaBMzyU6K93Ah3GdtTKe4LD+9U5j3Li5RJ6GAJ2EmWfl1eDQBM+by7HwNnYln3mbMr46D25EfB3bV1I0T4VVnZTRQU0fkzllI8oSFcbrJH6XZ7/3kOBhrf7vGz5XWExpbVGbZXfwb4/OqLFrekVsqP7Zkld+UxWJI8r593PoielOu7OzcjuIi9qy45scBl/AHIrczf4X7Uj3aRUFLLBIam7JivSDlLeuFkO9NMpQ3Rr8o5vViTY7pTw"
}"#;

async fn setup_broker() -> mockito::ServerGuard {
    let mut server = Server::new_async().await;
    {
        let cfg = config::get();
        let mut cfg = cfg.write().unwrap();
        cfg.use_proxy_protocol = Some(false);
        cfg.broker_auth_token = "test_token".to_string();
        cfg.dangerous_disable_ssl_verify = Some(false);
        cfg.ticket_api_url = server.url() + "/";
        cfg.max_sessions = None;
        cfg.max_sessions_per_remote = None;
        // NOTE: do not touch `udp_enabled` here — the UDP e2e tests rely on the
        // default and this global is not reset by the shared test setup.
    }
    server
        .mock("POST", "/")
        .match_header(TUNNEL_AUTH_HEADER, Matcher::Regex("Bearer sk-".into()))
        .match_body(Matcher::PartialJson(
            serde_json::json!({"command": "start"}),
        ))
        .with_status(200)
        .with_body(BROKER_RESPONSE)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_header(TUNNEL_AUTH_HEADER, Matcher::Regex("Bearer sk-".into()))
        .match_body(Matcher::PartialJson(serde_json::json!({"command": "stop"})))
        .with_status(200)
        .with_body("{}")
        .create_async()
        .await;
    server
}

fn client_crypts(ticket: &Ticket) -> (Crypt, Crypt) {
    let secret = SharedSecret::from_hex(KNOWN_SECRET_HEX).unwrap();
    let material = derive_tunnel_material(&secret, ticket).unwrap();
    // out = launcher->server, in = server->launcher
    (
        Crypt::new(&material.key_receive, 0),
        Crypt::new(&material.key_send, 0),
    )
}

#[derive(Debug, Clone, Copy)]
enum Abuse {
    /// Handshake only, never echo the ticket (server hits the 1 s confirm timeout).
    DropBeforeConfirm,
    /// Full handshake, then drop the socket (EOF teardown path, 5 s grace).
    DropAfterConfirm,
    /// Full handshake, then a proper `Close` control command.
    Close,
    /// Full handshake, then a `Nop` (keep-alive) and drop.
    Nop,
    /// Full handshake, then a non-UTF8 garbage frame on the control channel.
    Garbage,
    /// Full handshake, then `CloseChannel{1}` for a channel never opened.
    CloseUnoppened,
}

/// One iteration: drive a session with  and report whether the session
/// table came back to  within a bounded wait.
async fn one_iteration(abuse: Abuse) -> bool {
    let ticket = Ticket::new(TICKET_ID.as_bytes().try_into().unwrap());
    let manager = SessionManager::get_instance();
    let baseline = manager.count();

    let server = setup_broker().await;
    let (mut client, server_side) = tokio::io::duplex(128 * 1024);
    let (reader, writer) = tokio::io::split(server_side);
    let task = tokio::spawn(async move {
        let _ = handle_connection(reader, writer, "127.0.0.1:0".parse().unwrap()).await;
    });

    let (mut out, mut inc) = client_crypts(&ticket);

    let mut hs = Vec::new();
    hs.extend_from_slice(HANDSHAKE_V2_SIGNATURE);
    hs.push(HandshakeCommand::Open.into());
    hs.extend_from_slice(ticket.as_ref());
    client.write_all(&hs).await.ok();

    if !matches!(abuse, Abuse::DropBeforeConfirm) {
        out.write(&mut client, 1, ticket.as_ref()).await.ok();
        // Read the OpenResponse (it is one encrypted frame on any channel).
        let mut buffer = PacketBuffer::new();
        let _ =
            tokio::time::timeout(Duration::from_secs(3), inc.read(&mut client, &mut buffer)).await;

        let msg = match abuse {
            Abuse::Close => Some(Command::Close.to_message()),
            Abuse::Nop => Some(Command::Nop.to_message()),
            Abuse::CloseUnoppened => Some(Command::CloseChannel { channel_id: 1 }.to_message()),
            _ => None,
        };
        if let Some(msg) = msg {
            out.write(&mut client, 0, msg.payload.as_ref()).await.ok();
        }
        if matches!(abuse, Abuse::Garbage) {
            out.write(&mut client, 0, &[0xFFu8, 0xFE, 0xFD, 0xFC])
                .await
                .ok();
        }
    }

    drop(client); // abrupt close
    task.abort();

    // Abrupt drops take the designed 5 s recovery grace before the session is
    // reaped, so allow for it; the loop still exits as soon as the table drains.
    let tries = 120;
    for _ in 0..tries {
        if manager.count() <= baseline {
            drop(server);
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let after = manager.count();
    eprintln!("LEAK: abuse={abuse:?} baseline={baseline} after={after}");
    drop(server);
    false
}

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn teardown_never_leaks_a_session() {
    for abuse in [
        Abuse::DropBeforeConfirm,
        Abuse::Close,
        Abuse::Nop,
        Abuse::Garbage,
        Abuse::CloseUnoppened,
        Abuse::Close,
        Abuse::DropAfterConfirm,
    ] {
        assert!(one_iteration(abuse).await, "session leaked after {abuse:?}");
    }
}

/// Two sessions open at once, each torn down independently: neither the other's
/// teardown nor the shared proxy/manager state may keep a slot alive.
#[serial_test::serial(config, manager)]
#[tokio::test]
async fn two_concurrent_sessions_both_reap() {
    let ticket = Ticket::new(TICKET_ID.as_bytes().try_into().unwrap());
    let manager = SessionManager::get_instance();
    let baseline = manager.count();

    let server = setup_broker().await;
    let mut clients = Vec::new();
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let (client, server_side) = tokio::io::duplex(128 * 1024);
        let (reader, writer) = tokio::io::split(server_side);
        tasks.push(tokio::spawn(async move {
            let _ = handle_connection(reader, writer, "127.0.0.1:0".parse().unwrap()).await;
        }));
        clients.push(client);
    }

    let mut crypts = Vec::new();
    for client in clients.iter_mut() {
        let (mut out, mut inc) = client_crypts(&ticket);
        let mut hs = Vec::new();
        hs.extend_from_slice(HANDSHAKE_V2_SIGNATURE);
        hs.push(HandshakeCommand::Open.into());
        hs.extend_from_slice(ticket.as_ref());
        client.write_all(&hs).await.unwrap();
        out.write(client, 1, ticket.as_ref()).await.unwrap();
        let mut buffer = PacketBuffer::new();
        let _ = tokio::time::timeout(Duration::from_secs(3), inc.read(client, &mut buffer)).await;
        crypts.push(out);
    }

    assert!(
        manager.count() >= baseline + 2,
        "expected two registered sessions, got {}",
        manager.count()
    );

    // Close both properly.
    for (client, out) in clients.iter_mut().zip(crypts.iter_mut()) {
        let msg = Command::Close.to_message();
        let _ = out.write(client, 0, msg.payload.as_ref()).await;
    }
    drop(clients);
    for t in tasks {
        t.abort();
    }

    let mut ok = false;
    for _ in 0..60 {
        if manager.count() <= baseline {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop(server);
    assert!(ok, "sessions leaked: {}", manager.count());
}
