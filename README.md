<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/lockup-dark.svg">
    <img src="docs/brand/lockup.svg" alt="evmhot by bloque" height="64">
  </picture>
</p>

# evmhot

evmhot administers EVM hot wallets for deposits: a deposit address per registered party, monitoring of
every arrival, gas from a faucet, and sweeps to a configured target. It is part of Bloque's open-source tech
family.

It runs as one self-contained service for **several networks at once** (for example Polygon and Base): one
process, one store, one HTTP API. The same deposit address works on every configured network. The crate is also a
library (`evmhot`) that another program can embed; the HTTP surface lives in the binary only.

[`WEBHOOK_SPEC.md`](WEBHOOK_SPEC.md) describes the webhook payloads.

## How it works

Per network, a **monitor** reads blocks and logs, stores deposits of admitted tokens (and native coin) to
registered addresses and enqueues a `deposit_detected` webhook. A **sweeper** funds gas from the **faucet** just in
time, moves the address's balance to the network's sweep target and enqueues `deposit_swept`. A shared **webhook
worker** delivers the outbox at least once. Keys come from two mnemonics held only in memory.

## Configuration

Secrets and process settings come from the environment; per-network settings come from `chains.toml`.

| Variable | Required | Meaning | Default |
|---|---|---|---|
| `MNEMONIC` | yes | Deposit mnemonic; derives every deposit address | |
| `FAUCET_MNEMONIC` | yes | Faucet mnemonic; pays sweep gas | |
| `CHAINS_CONFIG` | no | Path to `chains.toml` | `chains.toml` |
| `DATABASE_URL` | no | `postgres://...` URL, or a SQLite file (`sqlite:wallet.db`) for a quick local run | `sqlite:wallet.db` |
| `DB_TLS_MODE` | no | Postgres TLS: `verify-ca` (default) or `disable` for a local server | `verify-ca` |
| `DB_POOL_SIZE` / `DB_READ_POOL_SIZE` | no | Postgres pool size / SQLite read-pool size | |
| `PORT` | no | HTTP port | `3000` |
| `DEFAULT_CHAIN` | no | Network the default `/evm/block_number` routes act on; unset or unknown falls back to the first `[[chains]]` entry | first chain |
| `WEBHOOK_JWT_TOKEN` | no | Bearer token sent to webhook URLs and required by `/admin/*` | |
| `WEBHOOK_MAX_RETRIES`, `WEBHOOK_RETRY_DELAY_MS`, `WEBHOOK_RETRY_POLL_INTERVAL`, `WEBHOOK_RETRY_BATCH_SIZE`, `WEBHOOK_LEASE_SECONDS` | no | Webhook delivery tuning | `5`, `1000`, `30`, `50`, `60` |
| `RUST_LOG` | no | Log filter | `evmhot=info` |

`LEGACY_CHAIN` and the `migrate_*` binaries only serve the one-off redb/SQLite to Postgres imports.

### `chains.toml`

Copy [`chains.toml.example`](chains.toml.example). Each `[[chains]]` block configures one network: `name`,
`chain_id`, `rpc_url` (HTTP polling only), `treasury_address` (the default sweep target), `faucet_address`,
`existential_deposit`, a non-empty `allowed_token_addresses`, and optionally `token_treasuries` (a per-token sweep
target), `token_targets` (see below), `min_deposits`, `min_deposit_default`, `min_deposit_native`, `poll_interval` and
`block_offset_from_head`. Use a separate RPC key per network in production so one busy network cannot exhaust
another's quota.

A token's sweep target is, in order: its `[chains.token_targets."0x..."]` table, its `token_treasuries` address, or the
network's `treasury_address`. The native coin always uses `treasury_address`. A token is listed in `token_treasuries`
or in `token_targets`, not both.

| `token_targets` key | Meaning |
|---|---|
| `kind` | `"transfer"` (sweep to `address`) or `"hold"` (detect and notify, never sweep). Changing a held token to `"transfer"` and restarting releases its held deposits on the first pass. |
| `address` | Required for `transfer`; not allowed for `hold`. |
| `min` | Smallest sweep, in the token's minor units. A balance below it (or, with `per_sender`, one sender's unswept total) waits for later deposits. |
| `max` | Largest sweep. A larger amount is swept in several sweeps, each at most `max`; `deposit_swept` lists them all in `sweeps` and is sent once, after the last. A remainder below `min` is topped up from the previous part when both can stay in bounds, otherwise it stays in the deposit address. |
| `admits_only` | `true` when the address admits only this token. The process refuses to start if another token, the native coin or the network default target routes to that address; the error names the conflict. |
| `per_sender` | `true` when each sweep needs one refund address. A sweep moves one sender's unswept deposits (not the whole balance) and never mixes senders; deposits with no recorded sender (stored before senders were) are swept one at a time. |

Each sweep is one `transfer` call into the target. evmhot reads the sweep's coordinate (transaction hash, block number,
block hash, block-level log index of the `Transfer` into the target, amount) from the receipt and reports it in
`deposit_swept` ([`WEBHOOK_SPEC.md`](WEBHOOK_SPEC.md)). A sweep whose receipt shows a revert, or has no such `Transfer`
log, is a failure and is never reported.

## HTTP API

Bodies are JSON; errors are a status code with a plain-text body. The surface is meant for an internal network:
`/evm/register` and `/evm/verify_transfer` are unauthenticated.

| Route | Request | Response |
|---|---|---|
| `GET /evm/health` | | `200 {"status": "OK (chains: ...)", "enabled": true}`; `status` is `"error"` when the store's writer is down |
| `POST /evm/register` | `{"id", "webhook_url"}` | `{"address"}` (same address for the same `id`); `503` while the store's write queue is full |
| `POST /evm/verify_transfer` | `{"chain", "tx_hash", "to_address", "amount", "token_type", "token_address"?, "token_symbol"?}` | `{"status": "success", "actual_to", "actual_amount", "token_type", ...}` or `{"status": "error", "message", ...}`; reads the chain, writes nothing |
| `GET /evm/block_number` | | `{"block_number"}`: the default network's cursor |
| `POST /evm/block_number` | `{"block_number"}` | `{"block_number"}`: moves the default network's cursor |
| `GET /evm/chains/{chain}/block_number` | | `{"chain", "block_number"}` |
| `POST /evm/chains/{chain}/block_number` | `{"block_number"}` | `{"chain", "block_number"}` |
| `POST /admin/retry_sweeps` | `{"chain", "tx_hash", "log_index"?}` | `{"retried", "token_type"}` |
| `POST /admin/retry_webhooks` | `{"id", "event"}` | `{"retried", "status"}` |

`/admin/*` needs `Authorization: Bearer <WEBHOOK_JWT_TOKEN>`. Moving a cursor backwards replays detection; deposits
are recorded at most once, and webhooks already delivered are not sent again.

## Run it locally with public RPCs

The development mnemonics in `env.docker.example` are the well-known anvil/hardhat ones. They are public; never
send real funds to their addresses.

```sh
cp env.docker.example .env
cat > chains.toml <<'TOML'
[[chains]]
name = "polygon"
chain_id = 137
rpc_url = "https://polygon-bor-rpc.publicnode.com"
treasury_address = "0x2222222222222222222222222222222222222222"   # placeholder
faucet_address = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"      # address of the dev faucet mnemonic
existential_deposit = "10000000000000000"
allowed_token_addresses = ["0xc2132D05D31c914a87C6611C10748AEb04B58e8f"]   # USDT on Polygon

[[chains]]
name = "base"
chain_id = 8453
rpc_url = "https://mainnet.base.org"
treasury_address = "0x1111111111111111111111111111111111111111"   # placeholder
faucet_address = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
existential_deposit = "10000000000000000"
allowed_token_addresses = ["0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"]   # USDC on Base
TOML
docker compose up --build        # evmhot + Postgres on http://localhost:3000
curl -s localhost:3000/evm/health
curl -s -X POST localhost:3000/evm/register -H 'content-type: application/json' \
  -d '{"id":"party-1","webhook_url":"https://example.com/hook"}'
```

Or without Docker: `cp .env.example .env`, put the same `chains.toml` in place, and `cargo run --bin evmhot` (SQLite
file by default).

Public RPC endpoints are rate limited; they are for trying the service out, not for production.

## Container image

CI publishes `ghcr.io/bloque-app/evmhot` on every push to `main` (tags `main` and `sha-<commit>`) and on `v*` tags.
The image runs as a non-root user and holds only the `evmhot` binary; migrations and the CA bundle are compiled in.
Mount your `chains.toml` at `/etc/evmhot/chains.toml` (or set `CHAINS_CONFIG`) and pass the secrets as environment.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test        # the Postgres backend tests need Docker; they skip when it is not available
```

As a library: `HotWalletService::new(config)`, `start_background_services()`, `register`, `verify_transfer`,
`get_block_number_async`, `set_block_number`, `retry_sweep`, `retry_webhook`.
