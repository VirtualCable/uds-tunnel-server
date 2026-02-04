// // BSD 3-Clause License
// // Copyright (c) 2026, Virtual Cable S.L.
// // All rights reserved.
// //
// // Redistribution and use in source and binary forms, with or without
// // modification, are permitted provided that the following conditions are met:
// //
// // 1. Redistributions of source code must retain the above copyright notice,
// //    this list of conditions and the following disclaimer.
// //
// // 2. Redistributions in binary form must reproduce the above copyright notice,
// //    this list of conditions and the following disclaimer in the documentation
// //    and/or other materials provided with the distribution.
// //
// // 3. Neither the name of the copyright holder nor the names of its contributors
// //    may be used to endorse or promote products derived from this software
// //    without specific prior written permission.
// //
// // THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// // AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// // IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
// // DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
// // FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
// // DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
// // SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
// // CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
// // OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// // OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

// // Authors: Adolfo Gómez, dkmaster at dkmon dot com
// use std::sync::Arc;

// use shared::{
//     consts,
//     crypt::{build_header, consts::HEADER_LENGTH, parse_header, types::SharedSecret},
//     system::trigger::Trigger,
//     ticket::Ticket,
// };

// use crate::session::{ClientEndpoints, Session, SessionId, SessionManager};

// use super::*;

// const TEST_CHANNEL_ID: u16 = 1; // Currently only supports channel 1

// fn make_test_crypts() -> (Crypt, Crypt) {
//     // Fixed key for testing
//     // Why 2? to ensure each crypt is used where expected
//     let key1 = SharedSecret::new([7; 32]);
//     let key2 = SharedSecret::new([8; 32]);

//     let inbound = Crypt::new(&key1, 0);
//     let outbound = Crypt::new(&key2, 0);

//     (inbound, outbound)
// }

// async fn create_test_server_stream() -> (Arc<Session>, tokio::io::DuplexStream) {
//     log::setup_logging("debug", log::LogType::Test);

//     let ticket = Ticket::new([0x40u8; consts::TICKET_LENGTH]);
//     let shared_secret = SharedSecret::new([3u8; 32]);

//     // Create the session
//     let session = Session::new(
//         shared_secret,
//         ticket,
//         Trigger::new(),
//         "127.0.0.1:0".parse().unwrap(),
//     );

//     // Add session to manager
//     let session = SessionManager::get_instance().add_session(session).unwrap();

//     let (client_side, tunnel_side) = tokio::io::duplex(1024);
//     let (tunnel_reader, tunnel_writer) = tokio::io::split(tunnel_side);

//     let tss = TunnelServerStream::new(*session.id(), tunnel_reader, tunnel_writer);

//     tokio::spawn(async move {
//         tss.run().await.unwrap();
//     });

//     (session, client_side)
// }

// async fn get_server_stream_components(
//     session_id: &SessionId,
// ) -> Result<(Trigger, ClientEndpoints, Crypt, Crypt)> {
//     let (stop, channels, inbound_crypt, outbound_crypt) =
//         if let Some(session) = SessionManager::get_instance().get_session(session_id) {
//             let (inbound_crypt, outbound_crypt) = session.server_tunnel_crypts()?;
//             (
//                 session.stop_trigger(),
//                 session.client_sender_receiver(TEST_CHANNEL_ID).await?,
//                 inbound_crypt,
//                 outbound_crypt,
//             )
//         } else {
//             log::warn!("Session {:?} not found, aborting stream", session_id);
//             anyhow::bail!("Session not found");
//         };
//     Ok((stop, channels, inbound_crypt, outbound_crypt))
// }

// async fn init_server_test() -> (
//     SessionId,
//     tokio::io::DuplexStream,
//     Trigger,
//     ClientEndpoints,
//     Crypt,
//     Crypt,
// ) {
//     log::setup_logging("debug", log::LogType::Test);

//     let (session, client) = create_test_server_stream().await;

//     let (stop, endpoints, inbound_crypt, outbound_crypt) =
//         get_server_stream_components(session.id()).await.unwrap();

//     (
//         *session.id(),
//         client,
//         stop,
//         endpoints,
//         inbound_crypt,
//         outbound_crypt,
//     )
// }

// #[serial_test::serial(manager)]
// #[tokio::test]
// async fn test_server_inbound_basic() {
//     log::setup_logging("debug", log::LogType::Test);

//     let (mut client, server) = tokio::io::duplex(1024);
//     let (mut crypt_in, mut _crypt_out) = make_test_crypts(); // Crypt out is for sending TO CLIENT

//     // Prepare encrypted message
//     let msg = b"16 length text!!";
//     let encrypted = {
//         let mut msg_packet = PacketBuffer::from_slice(msg);
//         let encrypted = crypt_in.encrypt(1, msg.len(), &mut msg_packet).unwrap();
//         encrypted.to_vec()
//     };
//     let counter = crypt_in.current_seq();
//     let length = encrypted.len() as u16;

//     let (tx, rx) = flume::bounded(10);
//     let stop = Trigger::new();

//     let mut inbound = TunnelServerInboundStream::new(server, crypt_in, tx, stop.clone());

//     let mut header = [0u8; HEADER_LENGTH];
//     build_header(counter, length, &mut header).unwrap();

//     tokio::spawn(async move {
//         client.write_all(&header).await.unwrap();
//         client.write_all(&encrypted).await.unwrap();
//         // Client will be closed automatically right here
//     });

//     inbound.run().await.unwrap();
//     let (channel_id, data) = rx.recv().unwrap();
//     log::debug!("Received data: {:?}:{:?}", channel_id, data);

//     assert_eq!(channel_id, TEST_CHANNEL_ID);
//     assert_eq!(data, msg);
//     // Stop is set on TunnelServerStream, so here must be unset
//     assert!(!stop.is_triggered());
// }

// #[serial_test::serial(manager)]
// #[tokio::test]
// async fn test_server_inbound_remote_close_before_header() {
//     log::setup_logging("debug", log::LogType::Test);

//     let (client, server) = tokio::io::duplex(1024);
//     let (crypt, _) = make_test_crypts();

//     let (tx, rx) = flume::bounded(10);
//     let stop = Trigger::new();

//     let mut inbound = TunnelServerInboundStream::new(server, crypt, tx, stop.clone());

//     drop(client);

//     inbound.run().await.unwrap();

//     assert!(rx.try_recv().is_err());
//     assert!(!stop.is_triggered());
// }

// #[serial_test::serial(manager)]
// #[tokio::test]
// async fn test_server_inbound_read_error() {
//     struct FailingReader;

//     impl tokio::io::AsyncRead for FailingReader {
//         fn poll_read(
//             self: std::pin::Pin<&mut Self>,
//             _cx: &mut std::task::Context<'_>,
//             _buf: &mut tokio::io::ReadBuf<'_>,
//         ) -> std::task::Poll<std::io::Result<()>> {
//             std::task::Poll::Ready(Err(std::io::Error::other("fail")))
//         }
//     }

//     log::setup_logging("debug", log::LogType::Test);

//     let (crypt, _) = make_test_crypts();
//     let (tx, _rx) = flume::bounded(10);
//     let stop = Trigger::new();

//     let mut inbound = TunnelServerInboundStream::new(FailingReader, crypt, tx, stop.clone());

//     let res = inbound.run().await;
//     assert!(res.is_err());
//     assert!(!stop.is_triggered());
// }

// #[serial_test::serial(manager)]
// #[tokio::test]
// async fn test_server_inbound_stop_before_read() {
//     log::setup_logging("debug", log::LogType::Test);

//     let (_client, server) = tokio::io::duplex(1024);
//     let (crypt, _) = make_test_crypts();

//     let (tx, rx) = flume::bounded(10);
//     let stop = Trigger::new();

//     let mut inbound = TunnelServerInboundStream::new(server, crypt, tx, stop.clone());

//     stop.trigger();

//     inbound.run().await.unwrap();

//     assert!(rx.try_recv().is_err());
// }

// #[serial_test::serial(manager)]
// #[tokio::test]
// async fn test_server_stream_with_invalid_packet() {
//     let (_session_id, mut client, stop, _endpoints, _inbound_crypt, _outbound_crypt) =
//         init_server_test().await;
//     // Send some data to tunnel, invalid data
//     let msg = b"Hello, client!";
//     client.write_all(msg).await.unwrap();

//     // Trigger should stop the stream on invalid packet
//     if !stop
//         .wait_timeout_async(std::time::Duration::from_secs(SERVER_RECOVERY_GRACE_SECS+1))
//         .await
//     {
//         panic!("Stream did not stop on invalid packet");
//     }
// }

// #[serial_test::serial(manager)]
// #[tokio::test]
// async fn test_server_stream_valid_packets() -> Result<()> {
//     let (_session_id, mut client, stop, endpoints, mut inbound_crypt, mut outbound_crypt) =
//         init_server_test().await;

//     // Note: This is a test, inbound is the crypt used to decrypt data coming FROM client
//     // So client will encrypt with our inbound (their outbound) crypt, and eill decrypt with our outbound (their inbound) crypt

//     // Prepare and send a valid packet to server inbound
//     let sent_msg = b"Hello, server!16";
//     let encrypted1 = {
//         let mut msg_packet = PacketBuffer::from_slice(sent_msg);
//         let encrypted = inbound_crypt
//             .encrypt(TEST_CHANNEL_ID, sent_msg.len(), &mut msg_packet)
//             .expect("Failed to encrypt first message packet");
//         encrypted.to_vec()
//     };
//     let counter1 = inbound_crypt.current_seq();
//     let length1 = encrypted1.len() as u16;

//     let mut header1 = [0u8; HEADER_LENGTH];
//     build_header(counter1, length1, &mut header1).unwrap();

//     client.write_all(&header1).await.unwrap();
//     client.write_all(&encrypted1).await.unwrap();

//     // Should receive on tx on time
//     let recv_msg =
//         tokio::time::timeout(std::time::Duration::from_secs(2), endpoints.rx.recv_async())
//             .await??;
//     assert_eq!(recv_msg, sent_msg);

//     // Stop should not be triggered
//     assert!(!stop.is_triggered());

//     // Now, send something that we will receibe crypted in client
//     let sent_msg2 = b"Another messa!16";
//     endpoints.tx.send_async((1, sent_msg2.into())).await?;

//     // Read from client the encrypted message
//     let mut header2 = [0u8; HEADER_LENGTH];
//     client.read_exact(&mut header2).await?;
//     let (counter2, length2) = parse_header(&header2)?;
//     let mut encrypted2 = vec![0u8; length2 as usize];
//     client.read_exact(&mut encrypted2).await?;

//     // Decrypt the message, contains the length
//     let mut packet_buffer2 = PacketBuffer::from_stream_slice(&encrypted2);
//     let (decrypted2, channel) = outbound_crypt
//         .decrypt(counter2, length2, &mut packet_buffer2)
//         .unwrap();

//     log::debug!(
//         "Decrypted message from server: decrypted: {:?} sent_msg2: {:?}",
//         decrypted2,
//         sent_msg2
//     );

//     assert_eq!(decrypted2, sent_msg2);
//     assert_eq!(channel, TEST_CHANNEL_ID); // Channel used in outbound stream

//     // Trigger stop to end the test
//     stop.trigger();
//     // Wait a bit and theres session should be closed
//     tokio::time::timeout(std::time::Duration::from_secs(1), async {
//         loop {
//             if SessionManager::get_instance()
//                 .get_session(&_session_id)
//                 .is_none()
//             {
//                 break;
//             }
//             tokio::time::sleep(std::time::Duration::from_millis(100)).await;
//         }
//     })
//     .await?;
//     Ok(())
// }
