# Server configuration reference

The tunnel server reads its configuration from a TOML file at startup.
The path is selected by build profile:

- Debug builds: `udstunnel.conf` in the current working directory.
- Release builds: `/etc/udstunnel.conf`.

A fully commented example is shipped at
[`docker/udstunnel.conf.example`](../docker/udstunnel.conf.example).

The `ServerConfig` struct (in
`crates/server/src/config/mod.rs`) is the source of truth for every
field accepted by the file. Unknown fields are ignored by
`serde::Deserialize`.

## Environment overrides

Three settings can be overridden via environment variables
without editing the file. They are applied once, when the
configuration is first read at startup; the effective config is
cached in a process-wide `OnceLock`, so editing the file or the
environment afterwards has no effect until the server is restarted.

| Variable                   | Overrides                  |
|----------------------------|----------------------------|
| `UDSTUNNEL_LISTEN_ADDR`    | `listen_addr`              |
| `UDSTUNNEL_LISTEN_PORT`    | `listen_port` (must parse) |
| `UDSTUNNEL_UDP_LISTEN_PORT`| `udp_listen_port` (must parse) |

## Fields

### Network

#### `listen_addr`

- Type: string
- Default: `*` (all interfaces)
- `*` is rewritten to `0.0.0.0` before binding. Any other value is
  parsed as a literal IP address.

#### `listen_port`

- Type: unsigned 16-bit integer
- Default: `443`
- Valid range: 1–65535. Ports below 1024 typically require elevated
  privileges on Linux.

#### `use_proxy_protocol`

- Type: boolean
- Default: `false`
- When `true`, the server expects a PROXY protocol v2 header on every
  incoming TCP connection before the UDS handshake. The header's
  source address is then used as the session `src_ip`. Leave `false`
  when the server is exposed directly to clients.

### UDP relay

#### `udp_enabled`

- Type: boolean
- Default: `true`
- Master switch for the UDP relay leg (used for RDP UDP redirection).
  The per-session gate is the broker's `enable_udp` flag on the ticket
  response; this field only controls whether the shared UDP socket is
  bound at all. If the bind fails, the server logs an error and keeps
  serving TCP only (RDP falls back to the TCP leg transparently).

#### `udp_listen_port`

- Type: unsigned 16-bit integer
- Default: same as `listen_port`
- Port of the shared UDP socket every session's UDP leg multiplexes on.
  The bind address always follows `listen_addr`. The resolved port is
  advertised to the client in the `OpenResponse` (`udp_port` field), so
  the launcher reaches the relay even when it is split from the TCP
  listener.

### Broker API

#### `ticket_api_url`

- Type: string
- Default: empty (the server will refuse to start if left empty in
  production; some offline tests populate a fake URL)
- Full URL of the broker REST endpoint that hands out tickets.

#### `broker_auth_token`

- Type: string
- Default: empty
- Bearer token sent in every broker request. Must match the value
  configured on the broker.

#### `dangerous_disable_ssl_verify`

- Type: boolean
- Default: `false`
- **Security-sensitive.** When `true`, the broker API client disables
  TLS certificate validation. Only intended for diagnostics against a
  broker presenting a self-signed certificate. Leaving this unset
  (or set to `false`) keeps validation on.
- The field name is deliberately prefixed `dangerous_` so that any
  search for the word turns the call site up; the HTTP client helper
  on the reqwest side is `danger_accept_invalid_certs`, which makes
  the boolean polarity identical between this config and the
  underlying transport.

### Logging

#### `log_level`

- Type: string
- Default: `info` in release, `debug` in debug builds
- Forwarded to the `tracing_subscriber` filter. Common values:
  `trace`, `debug`, `info`, `warn`, `error`.

### Sessions

#### `recovery_buffer_size`

- Type: unsigned integer (kilobytes)
- Default: `64`
- Size of the per-session recovery buffer used to replay packets to a
  reconnecting client. Larger values consume more memory per session
  but tolerate longer outages.

#### `max_sessions`

- Type: unsigned integer
- Default: `8192` (see `DEFAULT_MAX_SESSIONS` in
  `crates/server/src/consts.rs`)
- Hard cap on the number of concurrent sessions registered in the
  `SessionManager`. New Open handshakes are refused once the cap is
  hit. Bounds the worst-case cost of the O(N) lookup paths in
  `get_equiv_session` / `remove_equiv_session`.

#### `max_sessions_per_remote`

- Type: unsigned integer, optional
- Default: unset (disabled)
- When set, the connect path refuses to add a session when the
  number of currently registered sessions from the same source IP
  reaches this value. The handshake is stalled for one second before
  rejection so the client cannot distinguish the per-IP cap from a
  transient broker hiccup.
- The O(N) count runs only when this field is configured; the
  default config pays nothing for the check.
- **Choose a large value.** Behind NAT / CGNAT a single source IP is
  many users, not one: too low a cap silently locks out legitimate
  tenants that happen to share an egress address. The rejection log
  names the offending IP, the current count, the cap, and the
  `max_sessions_per_remote` knob to raise, so an operator can see at a
  glance whether it is abuse or legitimate shared load.

#### `session_idle_data_timeout_secs`

- Type: unsigned integer (seconds)
- Default: `120` (see `DEFAULT_SESSION_IDLE_DATA_TIMEOUT_SECS` in
  `crates/server/src/consts.rs`); `0` disables the cap
- Data-idle cap for sessions. A session that carries no payload bytes
  for this long is ended, freeing its slot. Keep-alive `Nop` frames do
  **not** count: they only refresh the launcher leg's 10 s TCP watchdog
  (`KEEPALIVE_TIMEOUT_SECS`), proving the socket is alive — they cannot
  sustain a session indefinitely. Real tunnel data in either direction
  and over either transport (TCP data channels, UDP relay datagrams)
  resets the clock. A `Recover` of the launcher stream does not reset
  it: the clock belongs to the session's traffic counters, not to the
  stream, so re-attaching a dataless launcher keeps it a dataless
  session. This is the defence against a client with a valid ticket
  parking a slot forever with `Nop`s only.

#### `rekey_seq_log2`

- Type: unsigned integer (`0..=63`)
- Default: `20` (see `DEFAULT_REKEY_LOG2` in `crates/shared/src/crypt/rekey.rs`)
- Rekeying threshold, as log2 of the frames per AES-GCM key epoch: every
  `2^k` sequence numbers (per direction, per transport), the tunnel re-derives
  a fresh key from the session's HKDF PRK with the frame's own epoch number,
  so a single key never protects more than `2^k` AES-GCM invocations (the
  NIST SP 800-38D per-key bound). `0` disables rekeying entirely (single key
  for the whole session lifetime, the pre-rekeying wire format); `1..=63`
  re-derives `epoch = seq >> k` deterministically, with no rekey handshake
  and no transition window. Values above `63` would make `seq >> k` undefined,
  so they are clamped to `63` with a warning instead of poisoning the
  handshake.
- **Rollout:** the threshold is owned by the server and adopted by the
  launcher through `OpenResponse.rekey_log2` (a byte added to the wire:
  90 -> 91 bytes). The change is **not backward-compatible**: a pre-rekeying
  launcher cannot parse the new `OpenResponse` (length mismatch), and the
  server never re-advertises a different `k` on `Recover` — the threshold is
  pinned to the session and survives stream replacements and re-connections.
- **Perf cost:** negligible. The re-derivation runs once per epoch boundary;
  `k = 20` means one HKDF expand + one AES key schedule per million frames
  per direction, and staying inside an epoch costs a comparison and a shift.

## Behaviour summary

| Concern                          | Knob                          | Default      |
|----------------------------------|-------------------------------|--------------|
| Bind                             | `listen_addr` / `listen_port` | `*` / 443    |
| UDP relay                        | `udp_enabled` / `udp_listen_port` | `true` / `listen_port` |
| PROXY v2 source IP               | `use_proxy_protocol`          | `false`      |
| Broker endpoint                  | `ticket_api_url`              | empty        |
| Broker auth                      | `broker_auth_token`           | empty        |
| TLS verification on broker       | `dangerous_disable_ssl_verify`| `false`      |
| Session recovery buffer          | `recovery_buffer_size`        | 64 KB        |
| Total session cap                | `max_sessions`                | 8192         |
| Per source-IP session cap        | `max_sessions_per_remote`     | disabled     |
| Data-idle session cap            | `session_idle_data_timeout_secs` | 120 s      |
| Key-epoch rekeying               | `rekey_seq_log2`               | 20         |

## Validation

The config struct is deserialised with `toml::from_str`. Parsing fails
loudly with a Rust-side error if a field has the wrong type. Missing
required fields (`ticket_api_url`, `broker_auth_token`) panic at
startup with a clear "Failed to parse server configuration file"
message. Panic unwinding is enabled in every build profile (no
`panic = "abort"` in any `[profile]` section), so release builds
behave the same as debug ones here: the process panics and the
runtime reports the unwound error.
