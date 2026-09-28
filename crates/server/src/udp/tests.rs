// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice,
//    this list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
//
// 3. Neither the name of the copyright holder nor the names of its contributors
//    may be used to endorse or promote products derived from this software
//    without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
// DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
// FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
// DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
// SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
// CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
// OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

// Authors: Adolfo Gómez, dkmaster at dkmon dot com

use super::*;

use shared::{
    crypt::{
        datagram::{DatagramCrypt, random_token},
        tunnel::get_udp_crypts,
        types::SharedSecret,
    },
    protocol::{consts::TICKET_LENGTH, ticket::Ticket},
};

use crate::session::UdpState;

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const NO_TRAFFIC_WINDOW: std::time::Duration = std::time::Duration::from_millis(300);

/// A session with a UDP leg whose keys derive from (secret, ticket), so a
/// test can build the launcher's mirror crypts with `get_udp_crypts`:
/// the launcher's send key is our inbound (c2s) key and vice versa.
async fn new_udp_session(remote: &str) -> (Arc<Session>, UdpToken, SharedSecret, Ticket) {
    let shared_secret = SharedSecret::new([7u8; 32]);
    let ticket = Ticket::new([3u8; TICKET_LENGTH]);
    let (inbound, outbound) = get_udp_crypts(&shared_secret, &ticket).unwrap();
    let token = random_token();
    let session = Arc::new(Session::new(
        shared_secret.clone(),
        ticket,
        Trigger::new(),
        "127.0.0.1:0".parse().unwrap(),
        vec![remote.to_string()],
    ));
    session.set_udp(UdpState::new(token, inbound, outbound));
    (session, token, shared_secret, ticket)
}

/// Launcher-side mirror crypts: send with the c2s key, receive with the
/// s2c key (both derivations are deterministic HKDF, so calling
/// `get_udp_crypts` twice yields the same key material).
fn launcher_crypts(
    shared_secret: &SharedSecret,
    ticket: &Ticket,
) -> (DatagramCrypt, DatagramCrypt) {
    let (send, _) = get_udp_crypts(shared_secret, ticket).unwrap();
    let (_, recv) = get_udp_crypts(shared_secret, ticket).unwrap();
    (send, recv)
}

/// Bind + spawn the relay as the process-global instance. Required by the
/// e2e tests: the handshake path registers each new session's token through
/// `SessionManager` -> `udp::register_session`, which resolves the
/// process-global relay.
async fn run_relay() -> (Arc<UdpRelay>, Trigger, SocketAddr) {
    let relay = UdpRelay::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = relay.local_addr().unwrap();
    let stop = Trigger::new();
    let task_relay = relay.clone();
    let task_stop = stop.clone();
    tokio::spawn(async move { task_relay.run(task_stop).await });
    (relay, stop, addr)
}

/// Hermetic variant of [`run_relay`]: drives its own relay without installing
/// the process-global `UDP_RELAY`, so the relay unit tests cannot clobber the
/// instance the e2e tests depend on. Those tests register sessions directly on
/// the returned relay, so they never need the global.
async fn run_relay_for_test() -> (Arc<UdpRelay>, Trigger, SocketAddr) {
    let relay = UdpRelay::bind_for_test("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = relay.local_addr().unwrap();
    let stop = Trigger::new();
    let task_relay = relay.clone();
    let task_stop = stop.clone();
    tokio::spawn(async move { task_relay.run(task_stop).await });
    (relay, stop, addr)
}

/// End to end: an encrypted datagram from the simulated launcher reaches
/// the simulated RDP host in cleartext, and the host's reply comes back
/// encrypted and decryptable by the launcher.
#[serial_test::serial(manager)]
#[tokio::test]
async fn relay_roundtrip_client_remote_client() {
    log::setup_logging("debug", log::LogType::Test);

    // Simulated RDP host: echoes a fixed reply for whatever it receives.
    let rdp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rdp_addr = rdp.local_addr().unwrap();
    let rdp_task = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let (len, peer) = rdp.recv_from(&mut buf).await.unwrap();
        let received = buf[..len].to_vec();
        rdp.send_to(b"rdp-reply", peer).await.unwrap();
        received
    });

    let (relay, stop, relay_addr) = run_relay_for_test().await;
    let (session, token, secret, ticket) = new_udp_session(&rdp_addr.to_string()).await;
    relay.register(&session);

    // Simulated launcher
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut client_send, mut client_recv) = launcher_crypts(&secret, &ticket);

    let datagram = client_send.encrypt(&token, b"rdp-udp-data").unwrap();
    client.send_to(&datagram, relay_addr).await.unwrap();

    // The remote must receive the payload in cleartext.
    let received = tokio::time::timeout(TIMEOUT, rdp_task)
        .await
        .expect("remote did not receive the forwarded payload")
        .unwrap();
    assert_eq!(received, b"rdp-udp-data");

    // And the remote's reply must come back to the launcher, encrypted.
    let mut buf = [0u8; 2048];
    let (len, _) = tokio::time::timeout(TIMEOUT, client.recv_from(&mut buf))
        .await
        .expect("client did not receive the remote reply")
        .unwrap();
    let reply = client_recv.decrypt(&token, &buf[..len]).unwrap();
    assert_eq!(reply.as_deref(), Some(b"rdp-reply".as_slice()));

    // The relay learned the client address from the valid datagram.
    let udp = session.udp().unwrap();
    assert_eq!(udp.client_addr(), Some(client.local_addr().unwrap()));

    assert_eq!(relay.counters.forwarded.load(Ordering::Relaxed), 1);
    assert_eq!(relay.counters.sent.load(Ordering::Relaxed), 1);

    stop.trigger();
}

/// A datagram carrying a token that no session owns must be discarded
/// without reaching any remote.
#[serial_test::serial(manager)]
#[tokio::test]
async fn relay_discards_unknown_token() {
    log::setup_logging("debug", log::LogType::Test);

    let rdp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rdp_addr = rdp.local_addr().unwrap();

    let (relay, stop, relay_addr) = run_relay_for_test().await;
    let (_session, _token, secret, ticket) = new_udp_session(&rdp_addr.to_string()).await;
    // Note: session deliberately NOT registered — its token is unknown.

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut client_send, _) = launcher_crypts(&secret, &ticket);
    let unknown_token = random_token();
    let datagram = client_send.encrypt(&unknown_token, b"intruder").unwrap();
    client.send_to(&datagram, relay_addr).await.unwrap();

    let mut buf = [0u8; 2048];
    assert!(
        tokio::time::timeout(NO_TRAFFIC_WINDOW, rdp.recv_from(&mut buf))
            .await
            .is_err(),
        "remote must not receive datagrams with unknown tokens"
    );
    // Give the relay a beat to process, then check the counter.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(relay.counters.discarded_unknown.load(Ordering::Relaxed), 1);
    assert_eq!(relay.counters.forwarded.load(Ordering::Relaxed), 0);

    stop.trigger();
}

/// Anti-amplification: while the client address is not authenticated,
/// data from the remote must never be sent anywhere. Once a client
/// address exists, the same path delivers.
#[serial_test::serial(manager)]
#[tokio::test]
async fn relay_never_sends_to_unauthenticated_client_addr() {
    log::setup_logging("debug", log::LogType::Test);

    let (relay, stop, _relay_addr) = run_relay_for_test().await;
    let (session, token, secret, ticket) = new_udp_session("127.0.0.1:9").await;
    let udp = session.udp().unwrap();

    // Simulated remote, with the per-session leg wired by hand (no
    // authenticated datagram has arrived, so `client_addr` is None).
    let rdp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let remote_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    remote_sock
        .connect(rdp.local_addr().unwrap())
        .await
        .unwrap();
    udp.set_remote_socket(remote_sock.clone());
    relay.spawn_return_task(&session, udp.clone(), remote_sock.clone());

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (_, mut client_recv) = launcher_crypts(&secret, &ticket);

    // The remote speaks first: must be dropped, not amplified anywhere.
    rdp.send_to(b"unsolicited", remote_sock.local_addr().unwrap())
        .await
        .unwrap();
    let mut buf = [0u8; 2048];
    assert!(
        tokio::time::timeout(NO_TRAFFIC_WINDOW, client.recv_from(&mut buf))
            .await
            .is_err(),
        "relay must not forward to an unauthenticated client address"
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(relay.counters.not_sent_no_addr.load(Ordering::Relaxed) >= 1);
    assert_eq!(relay.counters.sent.load(Ordering::Relaxed), 0);

    // Now the client authenticates (address learned) and the return path opens.
    udp.set_client_addr(client.local_addr().unwrap());
    rdp.send_to(b"solicited", remote_sock.local_addr().unwrap())
        .await
        .unwrap();
    let (len, _) = tokio::time::timeout(TIMEOUT, client.recv_from(&mut buf))
        .await
        .expect("client must receive once its address is authenticated")
        .unwrap();
    let payload = client_recv.decrypt(&token, &buf[..len]).unwrap();
    assert_eq!(payload.as_deref(), Some(b"solicited".as_slice()));

    stop.trigger();
}

/// The reaper tears down UDP legs idle for more than the timeout while
/// the owning TCP session stays alive, and drops tokens of dead sessions.
#[serial_test::serial(manager)]
#[tokio::test]
async fn reaper_clears_idle_udp_leg() {
    log::setup_logging("debug", log::LogType::Test);

    let relay = UdpRelay::bind_for_test("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let (session, token, _secret, _ticket) = new_udp_session("127.0.0.1:9").await;
    relay.register(&session);

    // Fresh leg survives the reap.
    relay.reap();
    assert!(session.udp().is_some());
    assert!(
        relay
            .sessions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&token)
    );

    // Force the leg idle beyond the timeout: next reap clears it.
    let udp = session.udp().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    udp.last_activity
        .store(now - UDP_IDLE_TIMEOUT_SECS - 1, Ordering::Relaxed);
    relay.reap();
    assert!(
        session.udp().is_none(),
        "idle UDP leg must be reaped while the session lives on"
    );
    assert!(
        !relay
            .sessions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&token)
    );

    // A token whose session was dropped is collected too.
    let (session2, token2, _, _) = new_udp_session("127.0.0.1:9").await;
    relay.register(&session2);
    drop(session2);
    // The Weak may still upgrade while other refs exist; ensure none do.
    relay.reap();
    assert!(
        !relay
            .sessions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&token2)
    );
}

// ────────────────────────────────────────────────────────────────────────
// End-to-end: real TCP handshake against `connection::handle_connection`
// with a mockito broker, then UDP datagrams through the global relay.
// ────────────────────────────────────────────────────────────────────────

mod e2e {
    use super::*;

    use base64::{Engine as _, engine::general_purpose};
    use mockito::Server;
    use tokio::io::AsyncWriteExt;

    use shared::{
        crypt::{
            Crypt,
            kem::{self, encapsulate},
            tunnel::derive_tunnel_material,
        },
        protocol::{consts::HANDSHAKE_V2_SIGNATURE, handshake::HandshakeCommand},
    };

    use crate::{config, connection::handle_connection, session::SessionManager};

    const E2E_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    /// Shared secret the fake broker hands out inside the encrypted
    /// ticket response (hex), same value the TCP-leg tests use.
    const TUNNEL_SECRET_HEX: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    // OpenResponse wire offsets (connection::types is private to that
    // module, so the e2e asserts on the raw layout directly):
    // session_id:48 | channel_count:u16 | inbound_seq:u64 | outbound_seq:u64 | udp_token:16 | udp_port:u16 | reserved:6
    const TOKEN_OFFSET: usize = TICKET_LENGTH + 2 + 8 + 8;
    const PORT_OFFSET: usize = TOKEN_OFFSET + TOKEN_LENGTH;
    const OPENRESPONSE_LEN: usize = PORT_OFFSET + 2 + 6;

    /// Builds the encrypted broker ticket response exactly as the real
    /// broker would: ML-KEM encapsulation against the server's (debug)
    /// public key + AES-256-GCM of the JSON payload under the derived
    /// payload key.
    fn build_broker_response(ticket: &Ticket, remote: SocketAddr, enable_udp: bool) -> String {
        let (_private, public_key_bytes) = kem::debug::get_debug_kem_keypair_768();
        let public_key = kem::PublicKey::from(&public_key_bytes);
        // Fixed encapsulation randomness: fine for a test vector.
        let (kem_ciphertext, kem_shared_secret) = encapsulate(&public_key, [7u8; 32]);

        let material = derive_tunnel_material(&kem_shared_secret.into(), ticket).unwrap();
        let inner = serde_json::json!({
            "remotes": [{ "host": remote.ip().to_string(), "port": remote.port() }],
            "notify": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "shared_secret": TUNNEL_SECRET_HEX,
            "enable_udp": enable_udp,
        });
        let data = Crypt::simple_encrypt(
            &material.key_payload,
            &material.nonce_payload,
            inner.to_string().as_bytes(),
        )
        .unwrap();

        serde_json::json!({
            "algorithm": "AES-256-GCM",
            "ciphertext": general_purpose::STANDARD.encode(kem_ciphertext.as_slice()),
            "data": general_purpose::STANDARD.encode(data),
        })
        .to_string()
    }

    /// Runs the full Open handshake over a duplex stream against a mockito
    /// broker whose ticket response carries the given `enable_udp` flag,
    /// and returns the raw OpenResponse bytes plus the client streams.
    async fn run_open_handshake(
        enable_udp: bool,
        remote: SocketAddr,
    ) -> (Vec<u8>, Ticket, tokio::io::DuplexStream) {
        let auth_token = "test_token";
        let ticket = Ticket::new_random();

        let mut server = Server::new_async().await;
        let _mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(build_broker_response(&ticket, remote, enable_udp))
            .create();

        {
            let config = config::get();
            let mut config = config.write().unwrap();
            config.use_proxy_protocol = Some(false);
            config.broker_auth_token = auth_token.to_string();
            config.dangerous_disable_ssl_verify = Some(false);
            config.ticket_api_url = server.url() + "/";
        }

        SessionManager::get_instance().finish_all_sessions().await;

        let (mut client_stream, server_stream) = tokio::io::duplex(4096);
        let fake_src_ip: SocketAddr = "127.0.0.1:0".parse().unwrap();
        tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(server_stream);
            if let Err(e) = handle_connection(reader, writer, fake_src_ip).await {
                log::error!("Server connection handling failed: {:?}", e);
            }
        });

        // Handshake: signature + Open + ticket
        let mut signature_buf = vec![0u8; HANDSHAKE_V2_SIGNATURE.len() + 1];
        signature_buf[..HANDSHAKE_V2_SIGNATURE.len()].copy_from_slice(HANDSHAKE_V2_SIGNATURE);
        signature_buf[HANDSHAKE_V2_SIGNATURE.len()] = HandshakeCommand::Open.into();
        signature_buf.extend_from_slice(ticket.as_ref());
        client_stream.write_all(&signature_buf).await.unwrap();

        // Client-side TCP crypts at (0, 0), same as the launcher's first pair
        let tunnel_secret = SharedSecret::from_hex(TUNNEL_SECRET_HEX).unwrap();
        let material = derive_tunnel_material(&tunnel_secret, &ticket).unwrap();
        let mut out_crypt = Crypt::new(&material.key_receive, 0);
        let mut in_crypt = Crypt::new(&material.key_send, 0);

        // Encrypted ticket echo on channel 1
        out_crypt
            .write(&mut client_stream, 1, ticket.as_ref())
            .await
            .unwrap();

        // Read the OpenResponse
        let (response, _channel) = tokio::time::timeout(E2E_TIMEOUT, async {
            let mut buffer = shared::crypt::types::PacketBuffer::new();
            in_crypt
                .read(&mut client_stream, &mut buffer)
                .await
                .map(|(data, channel)| (data.to_vec(), channel))
        })
        .await
        .expect("timed out waiting for OpenResponse")
        .expect("failed to read OpenResponse");

        assert_eq!(
            response.len(),
            OPENRESPONSE_LEN,
            "unexpected OpenResponse wire length"
        );
        (response, ticket, client_stream)
    }

    /// Full path: broker says `enable_udp: true` → OpenResponse carries a
    /// non-zero token → an encrypted launcher datagram reaches the fake
    /// RDP host in cleartext through the relay → the host's reply comes
    /// back encrypted and decryptable by the launcher.
    #[serial_test::serial(config, manager)]
    #[tokio::test]
    async fn e2e_udp_leg_after_open_handshake() {
        log::setup_logging("debug", log::LogType::Test);

        // Fake RDP host: echoes one reply for the first payload it gets.
        let rdp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let rdp_addr = rdp.local_addr().unwrap();
        let rdp_task = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (len, peer) = rdp.recv_from(&mut buf).await.unwrap();
            let received = buf[..len].to_vec();
            rdp.send_to(b"rdp-e2e-reply", peer).await.unwrap();
            received
        });

        // Bind the relay BEFORE the handshake so the SessionManager hook
        // registers the new session's token into this instance.
        let (relay, stop, relay_addr) = run_relay().await;

        let (response, ticket, _client_stream) = run_open_handshake(true, rdp_addr).await;

        let token: UdpToken = response[TOKEN_OFFSET..TOKEN_OFFSET + TOKEN_LENGTH]
            .try_into()
            .unwrap();
        assert_ne!(
            token, [0u8; TOKEN_LENGTH],
            "enable_udp=true must yield a non-zero token"
        );

        // The advertised UDP port must be the resolved relay port (the
        // config leaves `udp_listen_port` unset, so it falls back to the
        // TCP listen port), never zero.
        let udp_port =
            u16::from_be_bytes(response[PORT_OFFSET..PORT_OFFSET + 2].try_into().unwrap());
        assert_eq!(
            udp_port,
            config::get().read().unwrap().udp_sockaddr().port(),
            "OpenResponse must advertise the resolved UDP relay port"
        );
        assert_ne!(udp_port, 0);

        // The handshake alone must have registered the token in the relay.
        assert!(
            relay
                .sessions
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&token),
            "session token must be registered in the relay after connect"
        );

        // Simulated launcher UDP side: mirror crypts of the server pair.
        let tunnel_secret = SharedSecret::from_hex(TUNNEL_SECRET_HEX).unwrap();
        let (mut client_send, mut client_recv) = launcher_crypts(&tunnel_secret, &ticket);
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let datagram = client_send.encrypt(&token, b"rdp-e2e-data").unwrap();
        client.send_to(&datagram, relay_addr).await.unwrap();

        // Cleartext arrives at the fake RDP host...
        let received = tokio::time::timeout(E2E_TIMEOUT, rdp_task)
            .await
            .expect("RDP host did not receive the forwarded payload")
            .unwrap();
        assert_eq!(received, b"rdp-e2e-data");

        // ...and its reply comes back encrypted and decryptable.
        let mut buf = [0u8; 2048];
        let (len, _) = tokio::time::timeout(E2E_TIMEOUT, client.recv_from(&mut buf))
            .await
            .expect("launcher did not receive the RDP reply")
            .unwrap();
        let reply = client_recv.decrypt(&token, &buf[..len]).unwrap();
        assert_eq!(reply.as_deref(), Some(b"rdp-e2e-reply".as_slice()));

        // The session in the manager holds the UDP leg with the learned
        // authenticated client address.
        let session_id: Ticket = response[..TICKET_LENGTH].try_into().unwrap();
        let session = SessionManager::get_instance()
            .get_equiv_session(&session_id)
            .expect("session must exist in the manager");
        let udp = session.udp().expect("session must carry UDP state");
        assert_eq!(udp.client_addr(), Some(client.local_addr().unwrap()));

        stop.trigger();
        SessionManager::get_instance().finish_all_sessions().await;
    }

    /// With `enable_udp: false` in the broker response, the OpenResponse
    /// token must be all zero (UDP disabled) and the session must carry
    /// no UDP state, even though the server config allows UDP.
    #[serial_test::serial(config, manager)]
    #[tokio::test]
    async fn e2e_udp_disabled_by_broker_yields_zero_token() {
        log::setup_logging("debug", log::LogType::Test);

        let (relay, stop, _relay_addr) = run_relay().await;
        let rdp_addr: SocketAddr = "127.0.0.1:9".parse().unwrap(); // unreachable, never used

        let (response, _ticket, _client_stream) = run_open_handshake(false, rdp_addr).await;

        let token: UdpToken = response[TOKEN_OFFSET..TOKEN_OFFSET + TOKEN_LENGTH]
            .try_into()
            .unwrap();
        assert_eq!(
            token, [0u8; TOKEN_LENGTH],
            "enable_udp=false must yield a zero token"
        );
        let udp_port =
            u16::from_be_bytes(response[PORT_OFFSET..PORT_OFFSET + 2].try_into().unwrap());
        assert_eq!(udp_port, 0, "disabled UDP must advertise port 0");

        let session_id: Ticket = response[..TICKET_LENGTH].try_into().unwrap();
        let session = SessionManager::get_instance()
            .get_equiv_session(&session_id)
            .expect("session must exist in the manager");
        assert!(
            session.udp().is_none(),
            "session must not carry UDP state when the broker disables it"
        );
        assert!(
            relay
                .sessions
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "no token must be registered when UDP is disabled"
        );

        stop.trigger();
        SessionManager::get_instance().finish_all_sessions().await;
    }
}
