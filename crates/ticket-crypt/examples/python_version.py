#!/usr/bin/env python3

from cryptography.hazmat.primitives.kdf.hkdf import HKDF
from cryptography.hazmat.primitives import hashes

def derive_tunnel_material(shared_secret: bytes, ticket_id: bytes):
    """
    Derives keys and nonces for payload + tunnel from a KEM/Kyber shared_secret.

    shared_secret: bytes from KEM/Kyber (same on client and broker/tunnel)
    ticket_id: 48-byte unique ID for this ticket/session (used as HKDF salt)
    """

    if len(ticket_id) != 48:
        raise ValueError(f"ticket_id must be 48 bytes, got {len(ticket_id)}")

    hkdf = HKDF(
        algorithm=hashes.SHA256(),
        length=120,
        salt=ticket_id,
        info=b"openuds-ticket-crypt",
    )
    okm = hkdf.derive(shared_secret)  # 120 bytes

    key_payload   = okm[0:32]
    key_send      = okm[32:64]
    key_receive   = okm[64:96]
    nonce_send    = okm[96:108]   # 12 bytes
    nonce_receive = okm[108:120]  # 12 bytes

    return {
        "key_payload":   key_payload,
        "key_send":      key_send,
        "key_receive":   key_receive,
        "nonce_send":    nonce_send,
        "nonce_receive": nonce_receive,
    }

if __name__ == "__main__":
    # Example usage
    shared_secret = b'\x01' * 32  # Replace with actual shared secret (32 bytes)
    ticket_id = b'\x01' * 48      # Replace with actual ticket ID
    material = derive_tunnel_material(shared_secret, ticket_id)
    for k, v in material.items():
        print(f"{k}: {v.hex()}")
        print()

    print()
    print(material)
