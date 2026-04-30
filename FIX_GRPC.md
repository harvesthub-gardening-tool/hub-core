# gRPC fix — what changed and why

Notes for the team on the changes that made `InsertSensorData` work end-to-end on
the ESP32-S3, and the constraints that forced each decision. Read top to bottom
once; come back to the table at the end if you just need the deltas.

---

## TL;DR

Firmware now talks to the production API over **plaintext h2c on port 80**, using
a **hub JWT obtained at first boot via `auth.v2.ClaimHubToken`** (no Login flow
on the device, no end-user password ever stored), with timestamps in **Unix
milliseconds**. Hub identity (`device_id` + `hub_secret`) is generated on-device
and printed to serial as a QR payload for the mobile app to associate. We could
not use TLS because the `ring` crate has no Xtensa assembly, so the tonic TLS
stack does not build for `xtensa-esp32s3-espidf`.

---

## 1. Transport: plaintext h2c on port 80

### What we changed

- `tonic::transport::Channel` connects to `http://harvest-hub-api.grimmely.com`
  (no TLS).
- Backend serves gRPC on `:8080` over **h2c** (HTTP/2 cleartext) via
  `golang.org/x/net/http2/h2c`, fronted by Traefik on `:80`.
- HTTPS for the API was moved to a separate hostname
  (`harvest-hub-api-secure.grimmely.com`) so the firmware host is **not**
  redirected to TLS.

### Why

- `ring` (and `aws-lc-rs`) have no Xtensa assembly, so anything pulling them in
  fails to compile for `xtensa-esp32s3-espidf`. That includes every TLS-enabled
  feature of `tonic`/`hyper`/`rustls`.
- For a dev-stage hub on a controlled network, plaintext h2c is acceptable. The
  decision to keep TLS _off_ on this hostname is deliberate, not an oversight.
- HTTPS hardening is deferred. Likely path: terminate TLS at Traefik using a
  TLS implementation native to ESP-IDF (mbedTLS via `esp-idf-svc`) and a custom
  hyper connector. Out of scope for this fix.

### Decision boundary

- Production deployment of this firmware **must not** ship without TLS.
- Until then: do not point the firmware at any host that handles real user data
  outside the dev network.

---

## 2. Auth: hub-generated identity + one-shot `auth.v2.ClaimHubToken`

### What we changed

- The hub generates its own `device_id` (random UUIDv4) and `hub_secret`
  (32 random bytes hex, 64 chars) on first boot and persists both to NVS
  (namespace `hub`, keys `device_id` / `hub_secret`).
- It prints both as a "QR payload" block to the serial log. The owner scans
  that into the mobile app, which calls `auth.v2.AuthService/AssociateHub` with
  their user JWT, binding `(device_id, hub_secret)` to their account.
- After AssociateHub has run server-side, the hub calls
  `auth.v2.AuthService/ClaimHubToken(device_id, hub_secret)` exactly once.
  The server returns a long-lived hub JWT (RS256, 1-year expiry, empty
  username, `hub_id` claim populated). The hub stores it in NVS under `jwt`.
- All subsequent boots read the JWT from NVS and skip the claim step. The
  firmware sends it as `Authorization: Bearer <token>` on every gRPC call via
  a tonic `AuthInterceptor`.
- A build-time `HUB_TOKEN` env var is the dev-only override: if set, NVS is
  pre-seeded with that JWT and the generate / log / claim path is skipped
  entirely. Useful for local development against a wiped backend.

### Why

- A device should not store an end-user password. Compromising one hub would
  compromise the user's whole account.
- Hub-generated `(device_id, hub_secret)` removes the need for a build-time
  per-device provisioning step. The same firmware image can be flashed onto
  every hub; identity emerges at first boot from the on-board CSPRNG.
- `esp_fill_random` is only CSPRNG-quality once Wi-Fi or BLE is up, so
  `provision_hub` runs **after** `wifi::connect`. Reordering would silently
  downgrade entropy to pseudo-random — flagged in `persist.rs` and `main.rs`.
- Splitting AssociateHub (mobile, user JWT) from ClaimHubToken (hub, no auth)
  keeps the user's credentials off the device while still proving the user
  consented to this specific hub.
- Claim-once on the server side: a successful `ClaimHubToken` is recorded on
  the `hub_tokens` row, and any retry returns `FAILED_PRECONDITION`. The hub
  surfaces this as `auth::AlreadyClaimed` and halts with an instruction to
  call `auth.v2.RevokeHub` from the mobile app.
- Single round-trip per data RPC after provisioning: no Login step, no refresh
  dance, no token cache to rebuild on every boot.

### Decision boundary

- The 1-year expiry on hub JWTs is a backend constant. Renewal flow is not yet
  implemented; expect a re-claim cycle in ~12 months.
- `espflash erase-flash` wipes `device_id`, `hub_secret`, and the JWT. The
  next boot generates a fresh `device_id`, so the previous server-side
  binding becomes orphaned. Either set `HUB_TOKEN` for the next boot, or have
  the user `RevokeHub` + AssociateHub the new device_id.
- ClaimHubToken **must** run after SNTP sync; the issued JWT's `nbf` / `iat`
  are validated against the device clock on every subsequent call.

---

## 3. Runtime: dedicated `uplink` thread with its own Tokio runtime

### What we changed

- BLE scanning runs on the **main task** with `block_on` (esp32-nimble
  requires this; its types are `!Send`).
- Networking runs on a **separate OS thread** named `uplink`, with a 32 KB
  stack, owning a `tokio::runtime::Builder::new_current_thread()` runtime.
- Bridge between them: a bounded `std::sync::mpsc::sync_channel` of
  `SensorReading`. BLE side calls `tx.try_send` (non-blocking, drops when
  full). Uplink side does `rx.recv` and feeds the gRPC client.
- The gRPC `Channel` is created once and reused across calls; on RPC error we
  drop it and reconnect on next reading.
- At process start in `main()`:
    ```rust
    let _eventfs = esp_idf_svc::io::vfs::MountedEventfs::mount(5)?;
    ```
    This must outlive the runtime.

### Why

- **`!Send` BLE.** esp32-nimble bindings can't move across threads, so BLE
  cannot run inside a multi-thread Tokio runtime. We isolate it.
- **Stack budget.** Tonic + prost + h2 + the Tokio reactor easily blow the
  default ~8 KB pthread stack on ESP-IDF and produce silent stack-overflow
  panics. 32 KB is the empirically-tuned floor for our RPC paths.
- **EPERM on Tokio I/O.** Without `MountedEventfs::mount`, Tokio's reactor
  fails its first `eventfd()` syscall with `Permission denied (os error 13)`
  because ESP-IDF has no VFS handler registered for it. Mounting the VFS layer
  installs the handler.
- **`current_thread` runtime, not multi-thread.** ESP-IDF doesn't give us
  many threads cheaply, and we don't need work-stealing for one outbound
  channel.
- **Channel reuse.** A fresh tonic dial costs ~hundreds of ms; reuse keeps
  per-RPC latency at ~60–70 ms.
- **`sync_channel` (bounded).** Backpressure without blocking BLE. If the
  network is down, BLE keeps scanning; readings drop with a `warn!`. Lossy by
  design — the next reading is good enough.

### Decision boundary

- Don't try to "simplify" by collapsing BLE and networking onto one runtime.
  It will compile, then deadlock or stack-overflow on the device.

---

## 4. Timestamp unit: **Unix milliseconds**, end to end

### What we changed

- Firmware now emits timestamps in **ms** (`SystemTime::now()` → `as_millis()`).
- `src/time.rs`: function renamed `get_unix_now` → `get_unix_now_ms`, with a
  doc-comment pinning the cross-system contract.
- `src/main.rs` (fake-probe injection): multiply `sys::time()` (which returns
  seconds) by 1000 before sending.
- `garden.proto` already documented the field as `// Unix ms` — we now actually
  honor it.

### Why

- Server `InsertSensorData` does `time.UnixMilli(msg.Timestamp)`. The mobile
  app and the seed scripts already send ms. The firmware was the outlier,
  sending **seconds**, which the server then read as ms — landing every reading
  in **June 1970**.
- Symptom: `GetSummary` (default last 24 h) returned no firmware-originated
  rows. Data was in the DB the whole time, just dated 1970.
- We chose to fix the **firmware** rather than the server because changing the
  server would have broken the mobile app and the existing seed dataset, both
  of which already use ms.

### Decision boundary

- New clients added later must send **ms**. The proto comment is the authority
  (`int64 timestamp = 5; // Unix ms`).
- If we ever want to remove the ambiguity entirely, switch the proto field to
  `google.protobuf.Timestamp`. That requires regenerating `protos-rust` /
  `protos-go` and updating every client. Not done; flagged.

---

## 5. `fake-probe` cargo feature

### What we changed

- New optional feature in `Cargo.toml`: `fake-probe = []`. Off by default.
- When enabled, each BLE scan cycle injects one synthetic `SensorReading`
  (constant air temperature, air pressure, air humidity, soil temperature, soil
  humidity, and current timestamp) into the uplink channel — same code path as a
  real probe reading.

### Why

- Lets us validate the full firmware → gRPC → DB → `GetSummary` pipeline
  without any probe hardware in the room.
- Gated behind a feature flag so it can never accidentally ship in a release
  build. Use `cargo +esp build --release --features fake-probe` to enable.

### Decision boundary

- **Production builds must omit the flag.** No runtime check enforces this; it
  is a build-time discipline. CI for releases should reject any build that
  enabled `fake-probe`.

---

## 6. Config & secrets layout

### What we changed

- `.cargo/config.toml` is **untracked**. It holds Wi-Fi creds and (optionally)
  the dev `HUB_TOKEN` in its `[env]` block, consumed by `env!()` /
  `option_env!()` macros at compile time.
- `.cargo/config.toml.example` is committed, with placeholder values, and is
  the template a teammate copies.
- `.gitignore` now has the precise rule `/.cargo/config.toml` (not the broader
  `.cargo`, which would have stopped us tracking the example file).
- `Cargo.lock` is now **committed**. This is a binary crate; reproducible
  builds matter — a silent transitive bump in `tonic` / `prost` / `esp-idf-svc`
  could change binary size or stack usage on the device.

### Why

- `.cargo/config.toml` is the only file Cargo automatically loads `[env]` from
  during build, which is exactly what `env!("WIFI_SSID")` etc. need at compile
  time. Putting secrets there avoids a `build.rs` + `.env` loader.
- Untrack-then-template is the standard pattern for "tracked file with
  per-developer overrides". `git update-index --skip-worktree` was rejected:
  per-clone state, not in the repo, easy to forget.
- Binary crates pin `Cargo.lock` by Cargo team convention. Library crates
  don't.

### How to set up locally

```bash
cp .cargo/config.toml.example .cargo/config.toml
# edit WIFI_SSID, WIFI_PASSWORD, API_URL
# HUB_TOKEN is optional — leave commented out to exercise the production
# first-boot flow (hub generates creds, prints QR, calls ClaimHubToken).
```

For end-to-end provisioning without the mobile app, mimic its calls with curl:

```bash
# 1. Register / Login a user, save user_jwt
# 2. Boot the hub once; copy device_id + hub_secret from the serial log
# 3. POST /auth.v2.AuthService/AssociateHub with Authorization: Bearer <user_jwt>
#    body: {"device_id": "...", "hub_secret": "..."}
# 4. Reboot the hub — it calls ClaimHubToken and persists the JWT.
```

### Decision boundary

- Never commit `.cargo/config.toml`. The gitignore protects you, but visual
  review your staging area before any commit that touches `.cargo/`.
- If a real secret ever lands in a commit, **rotate** it; do not rely on
  history rewrites alone.
- `HUB_TOKEN` is a **dev-only** shortcut. Production builds must leave it
  unset so every device exercises the hub-generated identity flow.

---

## File-level summary

| File                         | Change                                                           | Reason                                            |
| ---------------------------- | ---------------------------------------------------------------- | ------------------------------------------------- |
| `src/time.rs`                | `get_unix_now` → `get_unix_now_ms` (`as_millis`)                 | Server expects ms                                 |
| `src/ble.rs`                 | Call site updated to `_ms` variant                               | Propagate unit fix                                |
| `src/main.rs`                | `MountedEventfs::mount(5)` at start of `main()`                  | Tokio I/O needs eventfd VFS                       |
| `src/main.rs`                | `uplink` worker thread (32 KB stack, current_thread Tokio)       | Isolate networking from `!Send` BLE               |
| `src/main.rs`                | `mpsc::sync_channel<SensorReading>` BLE → uplink                 | Backpressure without blocking BLE                 |
| `src/main.rs`                | `fake-probe` cfg-gated synthetic reading                         | End-to-end test without hardware                  |
| `src/grpc.rs`                | `HubClient::connect_with_token(&jwt)` (no Login)                 | Hub-scoped JWT obtained via auth.v2 ClaimHubToken |
| `src/grpc.rs`                | `AuthInterceptor` injects `Authorization: Bearer …`              | Auth on every RPC                                 |
| `src/grpc.rs`                | Reuse `Channel`, reconnect on RPC error                          | ~60 ms latency vs ~hundreds on fresh dial         |
| `src/auth.rs`                | `claim_hub_token` client + `AlreadyClaimed` error                | One-shot `auth.v2.ClaimHubToken` exchange         |
| `src/persist.rs`             | NVS `hub` namespace: `device_id`, `hub_secret`, `jwt`            | Persist hub identity + uplink JWT across reboots  |
| `Cargo.toml`                 | `[features] fake-probe = []`                                     | Opt-in synthetic injection                        |
| `sdkconfig.defaults`         | Bumped main / pthread stacks; lwIP MAX_SOCKETS, TCP_MSS, SND/WND | Avoid silent stack-overflow on TLS-less h2c paths |
| `.cargo/config.toml`         | **Untracked** (per-developer secrets via `env!`)                 | Don't commit Wi-Fi / hub token                    |
| `.cargo/config.toml.example` | **New, tracked**                                                 | Template for teammates                            |
| `.gitignore`                 | `.cargo` → `/.cargo/config.toml`                                 | Precise: ignore real config, allow example        |
| `Cargo.lock`                 | **Now tracked**                                                  | Reproducible firmware builds                      |

---

## Known follow-ups (not in this fix)

1. **Hub JWT renewal.** Tokens expire after 1 year. No automatic renewal flow
   exists today; expect to either re-claim (after the user calls RevokeHub) or
   add a refresh RPC before then.
2. **TLS for production.** Plaintext h2c is acceptable on the dev network only.
   Likely path: terminate TLS at Traefik using mbedTLS via `esp-idf-svc` and a
   custom hyper connector.
3. **Production build hygiene** that rejects `fake-probe` and unset `HUB_TOKEN`
   in release artifacts.
