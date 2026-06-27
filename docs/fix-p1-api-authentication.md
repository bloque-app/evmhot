# Fix (P1): Unauthenticated API — faucet drain and monitor tampering

Severity: P1 — financial (faucet drain) + availability (missed deposits)
Confidence: 7/10
Location: `src/api.rs:35-55`, `src/lib.rs:385-489` (`register` -> faucet), `src/lib.rs:130-138` (`set_block_number`)

## Problem

The server binds `0.0.0.0` with **no authentication on any route**:

```rust
let app = Router::new()
    .route("/health", get(health::<T>))
    .route("/register", post(register::<T>))
    .route("/verify_transfer", post(verify_transfer::<T>))
    .route("/block_number", get(get_block_number::<T>))
    .route("/block_number", post(set_block_number::<T>))
    .with_state(state);
let addr = format!("0.0.0.0:{}", port);
```

There is an outbound `webhook_jwt_token` for webhooks, but nothing protects inbound requests.

### Attack 1 — faucet drain (financial)

`POST /register` spawns a fire-and-forget faucet transfer of `existential_deposit` to every
newly derived address (`src/lib.rs:423-483`), with no auth and no rate limit. An attacker
loops `register` with random `id`s and drains the faucet wallet. Each call also writes an
account row and (with the collision bug) pollutes the index space.

### Attack 2 — monitor tampering (missed deposits)

`POST /block_number` calls `set_last_processed_block` (`src/lib.rs:131-133`,
`src/db.rs:177-185`). An attacker can **fast-forward** the last processed block; the monitor
then skips every block in the gap (`catch_up` starts at `last_processed`,
`src/monitor.rs:44-73`). Real deposits that land in skipped blocks are **never detected and
never swept** — funds arrive on-chain but the system is blind to them. Rewinding is less
harmful (re-scan; `record_deposit`/`record_erc20_deposit` dedup), but forwarding is a
silent money-loss lever.

## Fix

Add authentication to mutating routes. Two reasonable postures:

### Option A (recommended): shared-secret / JWT middleware

- Require an `Authorization: Bearer <token>` header on `register`, `verify_transfer`, and
  `POST /block_number`. Keep `/health` and (optionally) `GET /block_number` open.
- Reuse a config secret (a new `API_AUTH_TOKEN`, or validate the existing JWT). Reject with
  `401` when missing/invalid.
- Implement as an axum middleware layer so it is uniform and testable.

```rust
// sketch
async fn require_auth(headers: HeaderMap, req: Request, next: Next) -> Result<Response, StatusCode> {
    let ok = headers.get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(|t| constant_time_eq(t, expected_token))   // avoid timing leaks
        .unwrap_or(false);
    if ok { Ok(next.run(req).await) } else { Err(StatusCode::UNAUTHORIZED) }
}
```

### Option B: bind to localhost behind an authenticated gateway

- Bind `127.0.0.1` (config-driven) and terminate auth at a trusted reverse proxy.
- Simpler code, but pushes the security boundary to deployment config — easy to get wrong.

Additionally, regardless of option:

- **Rate-limit / gate `register`** so a flood cannot drain the faucet (per-caller limit, or
  require auth which implies a trusted caller).
- Consider validating `set_block_number` input (reject values far ahead of chain head) as
  defense-in-depth.

## Tests

- Mutating route without/with-bad token -> `401`; with valid token -> `200`.
- `/health` reachable without auth.
- `set_block_number` rejects a value far beyond current chain head (if bound added).

## Risk if not fixed

If the port is reachable beyond a trusted network, an attacker can empty the faucet and/or
blind the monitor to real deposits. Both are direct money loss.
