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
    crypt::types::SharedSecret,
    log,
    protocol::ticket::{self, TICKET_LENGTH, Ticket},
    system::trigger::Trigger,
};

async fn wait_for_session_existence(session_id: &SessionId, must_exists: bool) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let exists = SessionManager::get_instance()
                .get_session(session_id)
                .is_some();
            if exists == must_exists {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await?;
    Ok(())
}

async fn wait_for_session_manager_empty() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let empty = SessionManager::get_instance()
                .sessions
                .read()
                .unwrap()
                .is_empty();
            if empty {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await?;
    Ok(())
}

fn new_session_for_test(remote: &str) -> Session {
    Session::new(
        SharedSecret::new([0u8; 32]),
        ticket::Ticket::new_random(),
        Trigger::new(),
        "127.0.0.1:0".parse().unwrap(),
        vec![remote.to_string()],
    )
}

#[tokio::test]
async fn test_session_manager_add_and_get() {
    log::setup_logging("debug", log::LogType::Test);

    let manager = SessionManager::new();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();
    // Fail if session is not found
    assert_eq!(*session.shared_secret().as_ref(), [0u8; 32]);
    assert!(!session.is_server_running());
    assert!(session.is_running()); // Proxy should be running by default
    assert!(manager.get_session(session.id()).is_some());
    assert_eq!(manager.count(), 1);
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_session_running() -> Result<()> {
    log::setup_logging("debug", log::LogType::Test);
    // Session needs to be in global manager to be able to start server
    let session = new_session_for_test("127.0.0.1:1234");
    let session = SessionManager::get_instance().add_session(session)?;
    session.start_server().await.unwrap();
    assert!(session.is_running());
    assert!(session.is_server_running());
    Ok(())
}

#[tokio::test]
async fn test_session_sequence_numbers() {
    log::setup_logging("debug", log::LogType::Test);

    let session = new_session_for_test("127.0.0.1:1234");
    let seq = session.seqs();
    assert_eq!(seq, (0, 0));
    session.set_inbound_seq(5);
    session.set_outbound_seq(10);
    let seq = session.seqs();
    assert_eq!(seq, (5, 10));
}

#[serial_test::serial(manager)]
#[tokio::test]
#[ignore = "This test is to be executed 'manually' to check the SessionManager singleton behavior, not to be executed in CI"]
async fn test_get_session_manager() {
    log::setup_logging("debug", log::LogType::Test);

    let manager = SessionManager::get_instance();
    wait_for_session_manager_empty().await.unwrap();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();
    assert_eq!(*session.shared_secret().as_ref(), [0u8; 32]);
    // Clean up after test for other tests
    manager.sessions.write().unwrap().clear();
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_session_lifecycle() {
    log::setup_logging("debug", log::LogType::Test);

    let manager = SessionManager::get_instance();

    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();
    manager.start_server(session.id()).await.unwrap();
    assert!(session.is_running());
    assert!(session.is_server_running());
    assert!(manager.get_session(session.id()).is_some());

    manager.stop_server(session.id()).await;
    assert!(
        session
            .stop
            .wait_timeout_async(std::time::Duration::from_millis(500))
            .await
            .is_ok()
    );
    assert!(!session.is_server_running());
    wait_for_session_existence(session.id(), false)
        .await
        .unwrap();

    // No client is running in fact, and as the proxy is stopped,
    // but this should not fail
    manager.stop_client(session.id(), 1).await;
    wait_for_session_existence(session.id(), false)
        .await
        .unwrap();
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_session_removed_exactly_once() {
    log::setup_logging("debug", log::LogType::Test);

    let manager = SessionManager::get_instance();

    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();
    // Start servers first
    manager.start_server(session.id()).await.unwrap();
    assert!(manager.get_session(session.id()).is_some());

    manager.stop_server(session.id()).await;

    wait_for_session_existence(session.id(), false)
        .await
        .unwrap();

    // Any aditional stops should be no-ops
    manager.stop_server(session.id()).await;
    manager.stop_client(session.id(), 1).await;
}

#[tokio::test]
async fn test_get_session_returns_arc_clone() {
    log::setup_logging("debug", log::LogType::Test);

    let manager = SessionManager::new();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();

    let s1 = manager.get_session(session.id()).unwrap();
    let s2 = manager.get_session(session.id()).unwrap();

    assert!(Arc::ptr_eq(&s1, &s2));
}

#[tokio::test]
async fn test_get_equiv_session_default() {
    log::setup_logging("debug", log::LogType::Test);

    let manager = SessionManager::new();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();

    // The internal session id is no longer a valid equiv id: phase 2
    // moved the equiv id into a per-session `Option<Ticket>` that
    // starts as `None`. Only an id minted via `create_equiv_session`
    // (or the original ticket returned by the OpenResponse) should
    // resolve.
    assert!(manager.get_equiv_session(session.id()).is_none());
    assert!(manager.get_session(session.id()).is_some());
}

#[tokio::test]
async fn test_add_equiv_session() {
    let manager = SessionManager::new();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();

    let equiv_session_id = manager.create_equiv_session(session.id()).unwrap();
    let equiv_session = manager.get_equiv_session(&equiv_session_id).unwrap();
    let direct_session = manager.get_session(session.id()).unwrap();
    assert!(Arc::ptr_eq(&equiv_session, &direct_session));
}

#[tokio::test]
async fn test_remove_session_removes_equiv_session() {
    let manager = SessionManager::new();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();

    let equiv_session_id = manager.create_equiv_session(session.id()).unwrap();
    manager.remove_session(session.id());

    assert!(manager.get_equiv_session(&equiv_session_id).is_none());
    assert!(manager.get_session(session.id()).is_none());
}

#[tokio::test]
async fn test_remove_equiv_session() {
    let manager = SessionManager::new();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();

    let equiv_session_id = manager.create_equiv_session(session.id()).unwrap();

    manager.remove_equiv_session(&equiv_session_id);

    // Original session should still exist
    assert!(manager.get_session(session.id()).is_some());
    assert!(manager.get_equiv_session(&equiv_session_id).is_none());
}

/// Regression test for vuln-0005: `remove_session` must clear the
/// session's current equiv id so it stops resolving once the session
/// is gone. (Before phase 2, the manager kept a HashMap of equiv
/// entries that needed to be cleaned up; after phase 2 the equiv id
/// lives inside the Session and is dropped together with it.)
#[tokio::test]
async fn test_remove_session_clears_current_equiv_id() {
    let manager = SessionManager::new();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();

    let equiv_id = manager.create_equiv_session(session.id()).unwrap();
    assert!(manager.get_equiv_session(&equiv_id).is_some());

    manager.remove_session(session.id());

    assert!(manager.get_equiv_session(&equiv_id).is_none());
    assert!(manager.get_session(session.id()).is_none());
}

/// Regression test for vuln-0005: the recover pattern "remove old equiv,
/// mint new equiv" must not accumulate entries. Simulating the loop
/// directly on the manager (no broker / handshake needed) verifies that
/// the invariant holds no matter how many recovers happen.
#[tokio::test]
async fn test_equivs_do_not_accumulate_across_recoveries() {
    let manager = SessionManager::new();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();

    // Simulate 10 recovers: each one removes the previous equiv and mints
    // a fresh one. After all the recovers the manager must hold the
    // session itself plus at most one extra equiv id, not 10 of them.
    let mut previous: Option<SessionId> = None;
    for _ in 0..10 {
        if let Some(prev) = previous.take() {
            manager.remove_equiv_session(&prev);
        }
        previous = Some(manager.create_equiv_session(session.id()).unwrap());
    }

    // After phase 2 (per-session equiv id), the session carries exactly
    // one equiv id at any time. The 10 simulated recovers each
    // overwrite the previous one, so the total is 1, not 10.
    let session = manager.get_session(session.id()).unwrap();
    assert_eq!(
        session.current_equiv_id().as_ref(),
        previous.as_ref(),
        "session should hold the most recent equiv id"
    );

    // And only the latest equiv id still resolves.
    let latest = previous.expect("loop ran");
    assert!(manager.get_equiv_session(&latest).is_some());

    // None of the intermediate equiv ids should resolve.
    for i in 0..9 {
        let fake = Ticket::new([i as u8; TICKET_LENGTH]);
        assert!(
            manager.get_equiv_session(&fake).is_none(),
            "intermediate equiv id {i} unexpectedly still resolves"
        );
    }
}

/// Regression test for vuln-0005: a recover that removes its old equiv id
/// and mints a new one must leave exactly one live equiv for the session
/// (no idempotent self-entry in phase 2), and the old equiv must no
/// longer resolve.
#[tokio::test]
async fn test_recover_invalidates_old_equiv_id() {
    let manager = SessionManager::new();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();

    let old_equiv = manager.create_equiv_session(session.id()).unwrap();
    // Simulate the body of recover::recover: invalidate the inbound
    // equiv_session_id before minting a new one.
    manager.remove_equiv_session(&old_equiv);
    let new_equiv = manager.create_equiv_session(session.id()).unwrap();

    assert!(manager.get_equiv_session(&old_equiv).is_none());
    assert!(manager.get_equiv_session(&new_equiv).is_some());
    assert!(manager.get_session(session.id()).is_some());
}

/// Phase 2 invariant: a session owns at most one equiv_id at any time.
/// Calling `create_equiv_session` twice without an intermediate
/// `remove_equiv_session` must overwrite the previous equiv, not stack
/// a second one. This is the property that lets the manager drop its
/// global `HashMap<SessionId, SessionId>`: any "second mint" implicitly
/// supersedes the first, so no cleanup pass is needed.
#[tokio::test]
async fn test_create_equiv_session_twice_overwrites_previous() {
    let manager = SessionManager::new();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();

    let first = manager.create_equiv_session(session.id()).unwrap();
    // Intentionally skip `remove_equiv_session` — this is the path that
    // would have accumulated an extra entry in the old HashMap.
    let second = manager.create_equiv_session(session.id()).unwrap();

    assert_ne!(first, second, "equiv ids must differ between mints");

    // The session must point at the latest equiv (the only one it owns).
    assert_eq!(
        session.current_equiv_id(),
        Some(second),
        "session's equiv slot must hold the latest mint"
    );

    // The old equiv must no longer resolve, since the session has moved
    // on. This is the key behaviour change vs. the HashMap model: the
    // previous equiv is *gone*, not just out-of-date.
    assert!(
        manager.get_equiv_session(&first).is_none(),
        "first equiv id must be invalidated by the second mint"
    );
    assert!(
        manager.get_equiv_session(&second).is_some(),
        "second equiv id must resolve"
    );
}

/// Equiv ids are session-scoped: an equiv minted for session A must not
/// resolve via `get_equiv_session` when looked up from session B's
/// perspective. This guards against any future regression that, say,
/// stores the equiv in a flat map keyed only by equiv id without
/// verifying the owning session.
#[tokio::test]
async fn test_equiv_id_is_session_scoped() {
    let manager = SessionManager::new();
    let session_a = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();
    let session_b = manager
        .add_session(new_session_for_test("127.0.0.1:1235"))
        .unwrap();

    let equiv_a = manager.create_equiv_session(session_a.id()).unwrap();
    let equiv_b = manager.create_equiv_session(session_b.id()).unwrap();
    assert_ne!(equiv_a, equiv_b);

    // Each equiv resolves to its own session.
    let resolved_a = manager
        .get_equiv_session(&equiv_a)
        .expect("equiv_a must resolve");
    let resolved_b = manager
        .get_equiv_session(&equiv_b)
        .expect("equiv_b must resolve");
    assert_eq!(resolved_a.id(), session_a.id());
    assert_eq!(resolved_b.id(), session_b.id());

    // The session that owns an equiv must also report it from
    // `current_equiv_id` — and only that one must do so.
    assert_eq!(session_a.current_equiv_id(), Some(equiv_a));
    assert_eq!(session_b.current_equiv_id(), Some(equiv_b));
    assert_ne!(session_a.current_equiv_id(), session_b.current_equiv_id());
}
