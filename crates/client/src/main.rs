use std::time::Duration;

pub mod tunnelpq;

#[tokio::main]
async fn main() {
    let tunnel = tunnelpq::server::TunnelListener::new(
        tunnelpq::protocol::ticket::Ticket::new_random(), // Testing ticket while building
        [0u8; 32],
        Duration::from_secs(10),
        "127.0.0.1:8080".to_string(),
        shared::system::trigger::Trigger::new(),
    );
    tunnel.run().await.unwrap();
}
