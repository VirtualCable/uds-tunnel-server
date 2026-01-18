use super::*;

#[tokio::test]
async fn test_read_and_send() {
    let (mut client, server) = tokio::io::duplex(1024);
    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelClientInboundStream::new(server, tx, stop.clone());

    tokio::spawn(async move {
        client.write_all(b"hello").await.unwrap();
    });

    inbound.run().await.unwrap();

    assert_eq!(rx.recv().unwrap(), b"hello");
}

#[tokio::test]
async fn test_receive_and_write() {
    log::setup_logging("debug", log::LogType::Test);

    let (client, mut server) = tokio::io::duplex(1024);
    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut outbound = TunnelClientOutboundStream::new(client, rx, stop.clone());

    tokio::spawn({
        let stop = stop.clone();

        async move {
            log::debug!("Sending data through outbound stream");
            tx.send_async(b"hello".to_vec()).await.unwrap();
            // Wait for stop signal
            stop.wait_async().await;
        }
    });

    tokio::spawn(async move {
        log::debug!("Running outbound stream");
        outbound.run().await.unwrap();
    });

    // lee lo que escribió el outbound
    let mut buf = [0u8; 5];
    log::debug!("Waiting to read from server side");
    server.read_exact(&mut buf).await.unwrap();
    stop.trigger();
    log::debug!("Read data: {:?}", &buf);
    assert_eq!(&buf, b"hello");
}

#[tokio::test]
async fn test_inbound_remote_close() {
    let (client, server) = tokio::io::duplex(1024);
    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelClientInboundStream::new(server, tx, stop.clone());

    // Cerrar el lado remoto inmediatamente
    drop(client);

    inbound.run().await.unwrap();

    // No debe enviar nada
    assert!(rx.try_recv().is_err());
    assert!(stop.is_triggered());
}

#[tokio::test]
async fn test_inbound_read_error() {
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

    let (tx, _rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelClientInboundStream::new(FailingReader, tx, stop.clone());

    let res = inbound.run().await;
    assert!(res.is_err());
    assert!(stop.is_triggered());
}

#[tokio::test]
async fn test_outbound_channel_closed() {
    let (client, _server) = tokio::io::duplex(1024);
    let (_tx, rx) = flume::bounded::<Vec<u8>>(10);
    let stop = Trigger::new();

    drop(_tx); // cerrar canal

    let mut outbound = TunnelClientOutboundStream::new(client, rx, stop.clone());

    let res = outbound.run().await;
    assert!(res.is_err());
    assert!(stop.is_triggered());
}

#[tokio::test]
async fn test_outbound_stop_before_data() {
    let (client, _server) = tokio::io::duplex(1024);
    let (_tx, rx) = flume::bounded::<Vec<u8>>(10);
    let stop = Trigger::new();

    let mut outbound = TunnelClientOutboundStream::new(client, rx, stop.clone());

    stop.trigger(); // detener antes de arrancar

    outbound.run().await.unwrap();
    assert!(stop.is_triggered());
}

#[tokio::test]
async fn test_outbound_backpressure() {
    let (client, mut server) = tokio::io::duplex(1024);
    let (tx, rx) = flume::bounded::<Vec<u8>>(1);
    let stop = Trigger::new();

    let mut outbound = TunnelClientOutboundStream::new(client, rx, stop.clone());

    // Enviar dos mensajes: el segundo se bloqueará hasta que el primero se consuma
    tokio::spawn({
        let stop = stop.clone();
        async move {
            tx.send_async(b"one".to_vec()).await.unwrap();
            tx.send_async(b"two".to_vec()).await.unwrap();
            stop.wait_async().await;
        }
    });

    tokio::spawn(async move {
        outbound.run().await.unwrap();
    });

    let mut buf = [0u8; 3];
    server.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"one");

    let mut buf2 = [0u8; 3];
    server.read_exact(&mut buf2).await.unwrap();
    assert_eq!(&buf2, b"two");

    stop.trigger();
}

#[tokio::test]
async fn test_full_tunnel_echo() {
    let (client_side, mut server) = tokio::io::duplex(1024);
    let (server_side, mut client) = tokio::io::duplex(1024);

    let (tx_in, rx_in) = flume::bounded::<Vec<u8>>(10);
    let (tx_out, rx_out) = flume::bounded::<Vec<u8>>(10);

    let stop = Trigger::new();

    let mut inbound = TunnelClientInboundStream::new(server_side, tx_in, stop.clone());
    let mut outbound = TunnelClientOutboundStream::new(client_side, rx_out, stop.clone());

    // Task inbound
    tokio::spawn(async move {
        inbound.run().await.unwrap();
    });

    // Task outbound
    tokio::spawn(async move {
        outbound.run().await.unwrap();
    });

    // Simulate proxy that echoes back
    tokio::spawn({
        let stop = stop.clone();
        async move {
            while let Ok(msg) = rx_in.recv_async().await {
                tx_out.send_async(msg).await.unwrap();
            }
            stop.trigger();
        }
    });

    // Write on client side
    client.write_all(b"ping").await.unwrap();

    // Read b
    let mut buf = [0u8; 4];
    server.read_exact(&mut buf).await.unwrap();

    assert_eq!(&buf, b"ping");
}

#[tokio::test]
async fn test_inbound_multiple_packets() {
    log::setup_logging("debug", log::LogType::Test);

    let (mut client, server) = tokio::io::duplex(1024);
    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelClientInboundStream::new(server, tx, stop.clone());

    tokio::spawn(async move {
        inbound.run().await.unwrap();
    });

    tokio::spawn(async move {
        // Without delay, the three may come in a single read, or 2-1, etc.
        client.write_all(b"111").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await; // small delay to simulate separate packets
        client.write_all(b"222").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await; // small delay to simulate separate packets
        client.write_all(b"333").await.unwrap();
    });

    assert_eq!(rx.recv_async().await.unwrap(), b"111");
    assert_eq!(rx.recv_async().await.unwrap(), b"222");
    assert_eq!(rx.recv_async().await.unwrap(), b"333");
    log::debug!("All packets received");
    stop.trigger();
}

#[tokio::test]
async fn test_outbound_multiple_packets() {
    let (client, mut server) = tokio::io::duplex(1024);
    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut outbound = TunnelClientOutboundStream::new(client, rx, stop.clone());

    tokio::spawn({
        let stop = stop.clone();
        async move {
            tx.send_async(b"one".to_vec()).await.unwrap();
            tx.send_async(b"two".to_vec()).await.unwrap();
            tx.send_async(b"three".to_vec()).await.unwrap();
            stop.wait_async().await;
        }
    });

    tokio::spawn(async move {
        outbound.run().await.unwrap();
    });

    let mut buf = [0u8; 5];

    server.read_exact(&mut buf[..3]).await.unwrap();
    assert_eq!(&buf[..3], b"one");

    server.read_exact(&mut buf[..3]).await.unwrap();
    assert_eq!(&buf[..3], b"two");

    server.read_exact(&mut buf[..5]).await.unwrap();
    assert_eq!(&buf[..5], b"three");
    stop.trigger();
}
