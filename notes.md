# TODO
* Implement on Broker a mechanism to identify clients capabilities on get ticket requests (e.g. by using client version). Improve current implo to support POST requests with body containing client related info. Because we are not compatible with 4.0 version clients, we can simply switch to POST requests only.
* Add a mechanismt that, using the "public" KEM/Kyber key on the post, generate a shared secret between client and broker to encrypt the ticket response. This will avoid eavesdropping attacks. the ciphertext will be sent back to the client, which will be able to decrypt it using its private key to obtain the shared_secret and decrypt the data.
* The response ticket will contain:
   - launcher related data (data & js launcher script). Encirpted. (This may containt the tunnel server if is tunneled connection, and the second nonce)
   - the shared_secret, and a nonce
   - If tunnel connection:
        - the ticket_id for the tunnel connection
        - a second nonce
            - first nonce. used from data encryption, and already incremented by 1, 96 bits for the AES-GCM encryption from client to tunnel server
            - recv will be used (second nonce, 96 bits) for the AES-GCM encryption from tunnel server to client

* The tunnel ticket (one use only, deleted as soon as requested by the tunnel server) will contain (json serialized):
    - connection data (ip/port, ....)
    - the shared secret to encrypt the tunnel connection data (So we avoid eavesdropping attacks on the tunnel connection data also)
    - nonce for the tunnel connection (two nonces, one for each direction, first is the same that the one in the main ticket, incremented by one, second is a new nonce, 96 bits)
    - any other data that may be useful

The workflow will be:
1. Client requests a ticket to the broker (POST request with client kem public key, generated for every request, ephemeral)
2. Broker generates a shared secret and a ciphertext from the kem "public key" received
3. Broker generates the ticket (with launcher data and tunnel ticket data if needed), serializes it to json and encrypts it using the shared secret. If tuneled connection, also the tunnel ticket is generated and stores the shared_secret for the tunnel connection.
4. Client decrypts the ticket using its kem private and ciphertext received, using AES256-GCM with the shared secret, and a nonce that will be fixed (e.g. 12 bytes of zeroes, or data sent by the broker, whatever is more convenient)
5. Client uses the launcher data to launch the payload.
6. If tunneled connection, the client tunnel manager will process, using the shared secret, the two nonces and the ticket info. For this, every connection script (our authenticated js code, signed using ml-dsa) will contain the data that we receive, the secret and the first nonce. The second nonce will be part of the "data" that we receive from the ticket. so data, json serialized, will contain all the data from the broker + {..., "nonce": <first_nonce>, "secret": <shared_secret>}. Broker will never add these values to data sent to client, only the ticket will contain them.
Notes:
  Tunnel connects to broker, authenticated, to validate the ticket, get conneciton data, shared secret, nonce, etc.. The client will never sent these data directly to the tunnel server, only the ticket_id.
  
Keys:
    - We can use HKDF to derive the nonces, 2 keys (for assymetric encryption) from the shared secret generated using KEM.