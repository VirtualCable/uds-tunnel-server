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

Two settings can be overridden at runtime via environment variables
without editing the file. They take effect only when the
configuration file is read; later edits to the file still win.

| Variable                   | Overrides                  |
|----------------------------|----------------------------|
| `UDSTUNNEL_LISTEN_ADDR`    | `listen_addr`              |
| `UDSTUNNEL_LISTEN_PORT`    | `listen_port` (must parse) |

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

## Behaviour summary

| Concern                          | Knob                          | Default      |
|----------------------------------|-------------------------------|--------------|
| Bind                             | `listen_addr` / `listen_port` | `*` / 443    |
| PROXY v2 source IP               | `use_proxy_protocol`          | `false`      |
| Broker endpoint                  | `ticket_api_url`              | empty        |
| Broker auth                      | `broker_auth_token`           | empty        |
| TLS verification on broker       | `dangerous_disable_ssl_verify`| `false`      |
| Session recovery buffer          | `recovery_buffer_size`        | 64 KB        |
| Total session cap                | `max_sessions`                | 8192         |
| Per source-IP session cap        | `max_sessions_per_remote`     | disabled     |

## Validation

The config struct is deserialised with `toml::from_str`. Parsing fails
loudly with a Rust-side error if a field has the wrong type. Missing
required fields (`ticket_api_url`, `broker_auth_token`) panic at
startup with a clear "Failed to parse server configuration file"
message in debug builds and abort in release builds.
