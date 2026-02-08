use std::time::Duration;

use crate::tunnelpq::crypt::types::SharedSecret;

pub mod tunnelpq;

#[tokio::main]
async fn main() {
    let tunnel = tunnelpq::tunnel::Tunnel::new(
        tunnelpq::protocol::ticket::Ticket::new_random(), // Testing ticket while building
        SharedSecret::new([0; 32]),
        Duration::from_secs(10),
        "127.0.0.1:8080".to_string(),
        shared::system::trigger::Trigger::new(),
    );
    tunnel.run().await.unwrap();
}
