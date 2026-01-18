use super::*;

fn make_test_crypts() -> (Crypt, Crypt) {
    // Fixed key for testing
    // Why 2? to ensure each crypt is used where expected
    let key1: [u8; 32] = [7; 32];
    let key2: [u8; 32] = [8; 32];

    let inbound = Crypt::new(&key1);
    let outbound = Crypt::new(&key2);

    (inbound, outbound)
}

#[tokio::test]
async fn test_server_inbound_basic() {
    log::setup_logging("debug", log::LogType::Test);

    let (mut client, server) = tokio::io::duplex(1024);
    let (mut crypt_in, mut _crypt_out) = make_test_crypts(); // Crypt out is for sending TO CLIENT

    // Prepare encrypted message
    let msg = b"16 length text!!";
    let encrypted = {
        let mut msg_packet = PacketBuffer::from_slice(msg);
        let encrypted = crypt_in.encrypt(msg.len(), &mut msg_packet).unwrap();
        encrypted.to_vec()
    };
    let counter = crypt_in.current_seq();
    let length = encrypted.len() as u16;

    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelServerInboundStream::new(server, crypt_in, tx, stop.clone());

    let mut header = [0u8; HEADER_LENGTH];
    build_header(counter, length, &mut header).unwrap();

    tokio::spawn(async move {
        client.write_all(&header).await.unwrap();
        client.write_all(&encrypted).await.unwrap();
        // Client will be closed automatically right here
    });

    inbound.run().await.unwrap();

    assert_eq!(rx.recv().unwrap(), msg);
    // Stop is set on TunnelServerStream, so here must be not set
    assert!(!stop.is_set());
}

#[tokio::test]
async fn test_server_inbound_remote_close_before_header() {
    log::setup_logging("debug", log::LogType::Test);

    let (client, server) = tokio::io::duplex(1024);
    let (crypt, _) = make_test_crypts();

    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelServerInboundStream::new(server, crypt, tx, stop.clone());

    drop(client);

    inbound.run().await.unwrap();

    assert!(rx.try_recv().is_err());
    assert!(!stop.is_set());
}

#[tokio::test]
async fn test_server_inbound_read_error() {
    struct FailingReader;

    impl tokio::io::AsyncRead for FailingReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("fail")))
        }
    }

    log::setup_logging("debug", log::LogType::Test);

    let (crypt, _) = make_test_crypts();
    let (tx, _rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelServerInboundStream::new(FailingReader, crypt, tx, stop.clone());

    let res = inbound.run().await;
    assert!(res.is_err());
    assert!(!stop.is_set());
}

#[tokio::test]
async fn test_server_inbound_stop_before_read() {
    log::setup_logging("debug", log::LogType::Test);

    let (_client, server) = tokio::io::duplex(1024);
    let (crypt, _) = make_test_crypts();

    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelServerInboundStream::new(server, crypt, tx, stop.clone());

    stop.set();

    inbound.run().await.unwrap();

    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn test_server_stream() {
    log::setup_logging("debug", log::LogType::Test);

    use crate::{consts, session};
    let ticket = [0x40u8; consts::TICKET_LENGTH];

    // Create the session
    let session = session::Session::new([3u8; 32], ticket, Trigger::new());

    // Add session to manager
    let session_id = session::get_session_manager().add_session(session).unwrap();

    let (client_send, server_recv) = tokio::io::duplex(1024);
    let (server_send, client_recv) = tokio::io::duplex(1024);

    let tss = TunnelServerStream::new(session_id, client_recv, server_send);
}
