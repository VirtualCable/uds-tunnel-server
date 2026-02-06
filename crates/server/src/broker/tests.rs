use super::*;

use mockito::Server;
use shared::{protocol::consts::TICKET_LENGTH, crypt::kem::debug::get_debug_kem_keypair_768};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

pub const TICKET_ID: &str = "c6s9FAa5fhb854BVMckqUBJ4hOXg2iE5i1FYPCuktks4eNZD";

// Original JSON structure before encryption
// {
//     'remotes':
//         [
//             {'host': 'example.com', 'port': 12345},
//             {'host': 'example.com', 'port': 12345}
//         ],
//     'notify': 'notify_ticket',
//     'shared_secret': '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'
// }

const TICKET_RESPONSE_JSON: &str = r#"{
    "algorithm": "AES-256-GCM",
    "ciphertext": "CUVmJ3oeMEoa96hAOXI///Jm6H5QgRRikPOyy6B0arkVLt8fQZ3RiGKgeEB+1/srVFbEglK5t6xLLpsLYRu4ler9F88mSlzrce6BLNGxgySoY80198YwA2fww9NXGdN+3gI+qP056NxruqRlcrv/RqQLdbOncyq9xBCMw6HYNQmbd1NbVqObcphRZgJJAY84RgqZbmxAIcbt6hXNUcoXlCXQr3oinWxCbXiwLoGKqF2l4HTU3jEN991i0WCBqRoAi/uv1ctqBGVwFzgA+06azzdalqD9dRXIWvR7L82PULpsqWrhmfhyGJduOTTYA4/BT40rSaCBt9KGpEFsn+Ur+nUDnU48Sr6iMwz6/waP8ajEpqJkChjZ7UQPVtRlhvGMj1cTBg5l8sxMVIF6pLCI/w1uYdanGM2G+A/TxYStgg1Z0O2G8XIU2uVzsOPzwFtXz8/pu6YPPlt8ecqE9tmfGxhZdN5DGgQa1AmtY2R/fgCzQp4EcZ3d01D41UP55uSizH5WFL35gMOgoBEBlomvpk9CJU14AEj9GGVeCZwobdyxMfR1ZwSqA1lnC46D4T386tMh1+h25/vOUWCHcpttxY4SZsFZNW9Ca4t/2hxpAXluGGYeQ5noROK8Uc/s+Jv08kySw9OCXcSgHxtCGZ+SJSxkYQBtMm6hV2xZ5a+6yAItgnc75/ooHJT5APIoqh59TrskiNjGrXnR25w78+J6eEanK71QEjcQbriYh3bTHb3bb75DEs2ci4IKSy7B/DXB9EM22SqLbMR7/R22NAnzQ5tBoT+LKNMOO8mqYPSmQY95D5TPRP5yW7hd3NB4XI8nP5teBNSDNu9thMgGqceGYaRFgXGuq4pRtbKkEXyaj3WcJqM+DlFazmmKnricgvu5sTF4ekWm6iFf+JH8Mfw8ZPzwEVF5kT8Eq39OkOnJG1xscj5YnPndyVLHiHCiT+1tmFrVCp8iKKmyf8yL1Mg0RRW+S3m1Azeg3wl11gFGwVXAy4I+uBeGGTWjTkAty4TmcqlPPkg36YyQlrgTuUcbsgvJO0fnOuY3XU1XsOGi724dlQkJYqnXm6SYVI2CpwsHexcLLrBhBcG6SXWcFGGFNtBSRQOTa1+SPdwW1vKT12A4Bq5vIh25yr9EnqU5+Z77Q+sWEHl+/YQ/DOCSnloEyhF+K3LCBwU6W/crcVsNvbOZFJozCw5eFPRieMPpx846Q8bRNmv3zsPWk4dKkuIhAkpLuJOCbW7wIRH0v9h8BqX/8rRhg/PE4+grmfhxxOafkamwUD6bJ/8IuoKkmqloB5oZRWeQGAKzkdQznGLlbJftRcYQngsEt4hkyFg/sHR6rynjVVdOg01MXoWmG8gB0c3gVudNBYHsb4HaW0bWBlGENXM3+f0zpkNJlSifSpTQwTEIT+MEmAbGS5rdHKEZK+114u2ziPsDq1tyDTWCDNY=",
    "data": "+d2J4DoIgUYnKq0/WzhWvI4JlLzvjQIqMtxMiA3nsBZDDqoVRMJ5qrjLZEvf58+uu4M9cwv0Nfl+3Qn/6OO8x28z1KP5hgahNzA5qidTr2/nNaXHmgCqGeZ1GwuoJm1xOyMnAcsEHUWOCQDgrNWfXUXOeEL/r+QyfXZP4n5dkcQqs6WpYCJjzRIc/RYSF0+qSm0vtMzHrSnR5xlIIeX91yVWwpIeQSUwf9zXCUPoi07a8b7YNCa6QbnuSSWsxfum9Ki+jm2I2tFrmdCnDje0c0C5sertXzkc0ZaNyGg="
}"#;

async fn setup_server_and_api(auth_token: &str) -> (mockito::ServerGuard, HttpBrokerApi) {
    log::setup_logging("debug", log::LogType::Test);

    let server = Server::new_async().await;
    let url = server.url() + "/"; // For testing, our base URL will be the mockito server

    log::info!("Setting up mock server and API client");
    let (private_key, public_key) = get_debug_kem_keypair_768();
    // Store keys on /tmp for external checking
    std::fs::write("/tmp/kem_private_key_768_testing.bin", private_key).unwrap();
    std::fs::write("/tmp/kem_public_key_768_testing.bin", public_key).unwrap();

    let api = HttpBrokerApi::new(&url, auth_token, false).with_keys(private_key, public_key);
    // Pass the base url (without /ui) to the API
    (server, api)
}

#[tokio::test]
async fn test_http_broker() {
    let auth_token = "test_token";
    let (mut server, api) = setup_server_and_api(auth_token).await;
    let ticket: Ticket = TICKET_ID.as_bytes().try_into().unwrap();
    let ip = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(172, 27, 0, 1)), 0);
    let _m = server
        .mock("POST", "/")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(TICKET_RESPONSE_JSON)
        .create();
    let response = api.start_connection(&ticket, ip).await.unwrap();
    assert_eq!(response.remotes[0].host, "example.com");
    assert_eq!(response.remotes[0].port, 12345);
    assert_eq!(response.notify, "notify_ticket");
    assert_eq!(
        *response.get_shared_secret().unwrap().as_ref(),
        [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67,
            0x89, 0xab, 0xcd, 0xef
        ]
    );
}

#[tokio::test]
async fn test_http_broker_stop() {
    let auth_token = "test_token";
    let (mut server, api) = setup_server_and_api(auth_token).await;
    let ticket: Ticket = [b'A'; TICKET_LENGTH].into();
    let _m = server.mock("POST", "/").with_status(200).create();
    let result = api.stop_connection(&ticket).await;
    assert!(result.is_ok());
}
