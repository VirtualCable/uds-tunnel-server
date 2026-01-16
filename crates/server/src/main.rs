pub mod connection;
pub mod consts;
pub mod crypt;
pub mod log;
pub mod proxy_v2_protocol;
pub mod session;
pub mod stream;
pub mod system;

// TODO: implement real main
#[tokio::main(flavor = "multi_thread")]
async fn main() {
    log::setup_logging("debug", log::LogType::Tunnel);
    log::info!("Server started");
}
