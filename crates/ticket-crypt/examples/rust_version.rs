use hkdf::Hkdf;
use sha2::Sha256;

pub struct TunnelMaterial {
    pub key_payload: [u8; 32],
    pub key_send: [u8; 32],
    pub key_receive: [u8; 32],
    pub nonce_send: [u8; 12],
    pub nonce_receive: [u8; 12],
}

pub fn derive_tunnel_material(
    shared_secret: &[u8],
    ticket_id: &[u8],
) -> Result<TunnelMaterial, &'static str> {
    if ticket_id.len() < 48 {
        return Err("ticket_id must be at least 48 bytes");
    }

    // HKDF-Extract + Expand with SHA-256
    let hk = Hkdf::<Sha256>::new(Some(ticket_id), shared_secret);

    let mut okm = [0u8; 120];
    hk.expand(b"openuds-ticket-crypt", &mut okm)
        .map_err(|_| "HKDF expand failed")?;

    let mut key_payload = [0u8; 32];
    let mut key_send = [0u8; 32];
    let mut key_receive = [0u8; 32];
    let mut nonce_send = [0u8; 12];
    let mut nonce_receive = [0u8; 12];

    key_payload.copy_from_slice(&okm[0..32]);
    key_send.copy_from_slice(&okm[32..64]);
    key_receive.copy_from_slice(&okm[64..96]);
    nonce_send.copy_from_slice(&okm[96..108]);
    nonce_receive.copy_from_slice(&okm[108..120]);

    Ok(TunnelMaterial {
        key_payload,
        key_send,
        key_receive,
        nonce_send,
        nonce_receive,
    })
}

fn main() {
    // Example usage
    let shared_secret = [0x01u8; 32]; // Replace with actual shared secret
    let ticket_id = [0x01u8; 48]; // Replace with actual ticket ID

    match derive_tunnel_material(&shared_secret, &ticket_id) {
        Ok(material) => {
            println!("Key Payload: {:x?}", material.key_payload);
            println!("Key Send: {:x?}", material.key_send);
            println!("Key Receive: {:x?}", material.key_receive);
            println!("Nonce Send: {:x?}", material.nonce_send);
            println!("Nonce Receive: {:x?}", material.nonce_receive);
        }
        Err(e) => {
            eprintln!("Error deriving tunnel material: {}", e);
        }
    }
}
