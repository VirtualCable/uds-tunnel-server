use super::*;

use mockito::Server;
use shared::consts::TICKET_LENGTH;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};


// Pre checked base64 Kyber keys, ticket, etc.. for testing
const PRIVATE_KEY_768_TESTING: &str = "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";

const PUBLIC_KEY_768_TESTING: &str = "d7qwr9yBu2K0fpAAsYdTyHRCL+iLaxBf5/F019DFOFbC+HY8PgVOH+y2uicK/5td3lpfzjm302eDDxJ9\
8+O2Cpt9R7yQEWCOVWvH26Z2k2BgXSmsovxqvtu5xVKvLOkpKEZyHpGgiumpSxS+0TE5Dgh2eQBJhmSk\
rysKbFWB0QUCQXAaH7BbQMhftKfIJ0W7ckeNilyZDGh2dVlN7Ohl1Bq0L0wvgvNmprFne3uCYmddE2F1\
Xrh/rwbDZvzKf5WRT+DJStdIOVxSV8c4PHkLnls1/XllgaM/9uNEgMtyWKOBHDmYFqCWnsMMlgoB8IGD\
IJKuDCSJuVNQVBuLGiDDrVye16dJpOS9B0uBt3sntvgy/PKSooVuamq3A8k/LzGcFfw9fIOLccKfFhhI\
cbllN9y1C2kYBHo6Lxxb5yNPmUsIdFAsRkg5cWaHh/gA1uEAXDIzttF2WYJEZfej6Hd+dWAJfowOAEnA\
e0KPz2KZBxocJRhndjFp6gxpapesa1mmd2dhu7aqZptAbmCt5tUyXNsjguGCmbqQ7cMc5zKLQbu/U0K8\
i2SD1pxrMgxa4UkKsTVWeMUKKzufCllPdYiiLmAgIZUO3xxK0vsQ45Cq+9xhlWWG61olnlqyx9EoffQh\
3IAR1HO0twEXqCt+BNU5x2ue3cpgZhwWBLfDBpyRWpeW3eGkbHtLt9OYolQCOMROkLKMEyM1k/pfrtYO\
l6NDYbEKwJY3NdkKaFG5ROs5dbIcO1IZ8kyzMfhyTRYkNqLAW+ISRTl0+MdPaIU6MtVHA2h7fpsVw6Nx\
oaASIdEhNIaOy0NRIFZQILsvXSTM3pcHmYkk6QhgIcN/s/anc7KWNsS/TQsvuzXEwFS4GDsAv+rPG3jA\
vrh/MqMAgMAh2GWI9LBaYgFngrA3U2xNGho69jFG2QtcVgc93kyns1cQuGWSL2lixdDGcwUjTetjegNJ\
Uehiz2ZJ7qadQlNPvJOT1dpOcPWKH/moz6gNOrw2LrRHZAOLdiS6arcmCWm/Xhq8gznKn7k178w0lRat\
nGUspBOZDXMNedYjPvKLmGV01ZhUUcET6wWpsbjLLhdEpfMMICW4HZI9GamENvEEjZkHzjEXJGWSMFnI\
vcxLUaWJW/MOqMmjjaabctRzo7dCJ8dKAN2l03UMkkyjF2s1DKlVSkrK+6Mwnnt7smF1k3BWU0ctt5BG\
U8xjP2Vm4npBPTEhpwUJ6LsfZ4pLANddBDHELDAerLuPcWuVc/hnbVxAIUpYK3ASeWKi1gdfCymQWaI1\
5OTKSwtXGNUh7bObbmkzjRofhRgTLCmnXhgL9pmX1SUcfck1drunDjjAPPsl3oO299e17AV4CMMugYKX\
sJZwl6BekOYG1ygz1DgUB0mDPaFVDucPbokk8DMtLOrKu+Bu9rOa7+MdxwYJ+mMjLeae9+MURrdHwtYh\
vaSdDPA8O1h8sGhck2ewpLR3dGCHeyeJsEws1IiCnRa2cqxA+LxenDJV1WeGjWogd+MiNioBJ+kzpLpe\
AFohLlJG5zmHijRc/O4OYbzvH/NisQARHwX3ScApN7qAjG49j2fwJgcrKCM=";

const TICKET_ID: &str = "c6s9FAa5fhb854BVMckqUBJ4hOXg2iE5i1FYPCuktks4eNZD";


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

fn get_keypair() -> Result<([u8; PRIVATE_KEY_SIZE], [u8; PUBLIC_KEY_SIZE])> {
    let kem_private_key_bytes = general_purpose::STANDARD
        .decode(PRIVATE_KEY_768_TESTING)
        .map_err(|e| anyhow::format_err!("Failed to decode base64 KEM private key: {}", e))?;
    let kem_private_key_bytes: [u8; PRIVATE_KEY_SIZE] = kem_private_key_bytes
        .try_into()
        .map_err(|_| anyhow::format_err!("Invalid KEM private key size"))?;
    let kem_public_key_bytes = general_purpose::STANDARD
        .decode(PUBLIC_KEY_768_TESTING)
        .map_err(|e| anyhow::format_err!("Failed to decode base64 KEM public key: {}", e))?;
    let kem_public_key_bytes: [u8; PUBLIC_KEY_SIZE] = kem_public_key_bytes
        .try_into()
        .map_err(|_| anyhow::format_err!("Invalid KEM public key size"))?;
    Ok((kem_private_key_bytes, kem_public_key_bytes))
}

async fn setup_server_and_api(auth_token: &str) -> (mockito::ServerGuard, HttpBrokerApi) {
    log::setup_logging("debug", log::LogType::Test);

    let server = Server::new_async().await;
    let url = server.url() + "/"; // For testing, our base URL will be the mockito server

    log::info!("Setting up mock server and API client");
    let (private_key, public_key) = get_keypair().unwrap();
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
