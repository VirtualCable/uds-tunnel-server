use anyhow::Result;

struct Material {
    pub _not_used: [u8; 32],
    pub key_receive: [u8; 32],
    pub key_send: [u8; 32],
}

use hkdf::Hkdf;
use sha2::Sha256;
use super::Crypt;

fn derive_tunnel_material(
    shared_secret: &[u8],
    ticket_id: &[u8],
) -> Result<Material> {
    if ticket_id.len() < 48 {
        anyhow::bail!("ticket_id must be at least 48 bytes");
    }

    // HKDF-Extract + Expand with SHA-256
    let hk = Hkdf::<Sha256>::new(Some(ticket_id), shared_secret);

    let mut okm = [0u8; 96];
    hk.expand(b"openuds-ticket-crypt", &mut okm)
        .map_err(|_| anyhow::format_err!("HKDF expand failed"))?;

    let mut _not_used = [0u8; 32];
    let mut key_send = [0u8; 32];
    let mut key_receive = [0u8; 32];

    _not_used.copy_from_slice(&okm[0..32]);
    key_send.copy_from_slice(&okm[32..64]);
    key_receive.copy_from_slice(&okm[64..96]);

    Ok(Material {
        _not_used,
        key_receive: key_send,
        key_send: key_receive,
    })
}


pub fn get_tunnel_crypts(
    shared_secret: &[u8],
    ticket_id: &[u8],
) -> Result<(Crypt, Crypt)> {
    let material = derive_tunnel_material(shared_secret, ticket_id)?;

    let inbound = Crypt::new(&material.key_send);
    let outbound = Crypt::new(&material.key_receive);

    Ok((inbound, outbound))
}