# Webhook Specification

Multi-chain hot wallet: one registration address works on every configured EVM chain. Webhooks are sent per deposit/sweep event and include which chain the event occurred on.

All deposit and sweep webhooks include:
- `chain` — short name from `chains.toml` (e.g. `base`, `polygon`)
- `chain_id` — numeric EVM chain ID

Polling-only: there is no WebSocket streaming mode.

## Idempotency (`id` field)

- **Native deposits**: `{chain}:{tx_hash}` (e.g. `polygon:0xabc...`)
- **ERC20 deposits**: `{chain}:{tx_hash}:{log_index}` (e.g. `base:0xabc...:0`)
- **Native swept**: `{chain}:{original_tx_hash}`
- **ERC20 swept**: `{chain}:{tx_hash}:{log_index}`

## deposit_detected

```json
{
  "id": "polygon:0x1234...",
  "chain": "polygon",
  "chain_id": 137,
  "event": "deposit_detected",
  "account_id": "0x742d35Cc6634C0532925a3b844Bc454e4438f44e",
  "registration_id": "user_123",
  "tx_hash": "0x1234...",
  "from_address": "0x00000000000000000000000000000000000a11ce",
  "amount": "1000000",
  "token_type": "erc20",
  "token_symbol": "USDT",
  "token_address": "0xc2132d05d31c914a87c6611c10748aeb04b58e8f",
  "token_decimals": 6
}
```

`from_address` is the sender of the inbound transfer: the `from` of the `Transfer` log for a token, the `from` of the
transaction for the native coin. A consumer can use it as the refund address. It is `null` in `deposit_swept` for a
deposit recorded before senders were stored.

A token whose `symbol()` cannot be read is reported with `token_symbol: "UNKNOWN"` (and 18 decimals); it is handled like
any other admitted token. A token whose target is `hold` (see `chains.toml.example`) gets `deposit_detected` and no
`deposit_swept` until its target changes.

## deposit_swept

Same fields as detection (the transaction hash is `original_tx_hash`, not `tx_hash`; `from_address` is repeated). A
native deposit carries no sweep fields. A token deposit adds:

```json
{
  "sweep_tx_hash": "0xsweep1...",
  "sweeps": [
    {
      "sweep_tx_hash": "0xsweep1...",
      "sweep_block_number": 123456,
      "sweep_block_hash": "0xblock...",
      "sweep_log_index": 17,
      "sweep_amount": "1000000",
      "sweep_target": "0xTargetAddress..."
    }
  ]
}
```

- `sweeps` has one entry per sweep that settled the deposit: usually one, several when the amount was split by the
  target's `max`. Each entry is the sweep coordinate: the sweep transaction, its block, and the **block-level** log
  index of the `Transfer` into the target (from the receipt, not the log's position in the transaction), plus the amount
  that sweep moved, and `sweep_target`, the address the `Transfer` went to (`null` for sweeps recorded before the
  target was stored).
- The top-level `sweep_tx_hash` is the first entry's, so clients that read only it keep working.
- `deposit_swept` is sent once, after the deposit's last sweep succeeded and was verified on its receipt; until then
  only `deposit_detected` has been sent. A reverted sweep produces no event.
- Several deposits settled by one sweep (a whole-balance sweep) each get their own event, all naming the same entry. With
  `per_sender` a sweep covers one sender's deposits only; with `per_transfer`, one deposit only.
- A deposit whose sweep cannot be shown (its receipt has no `Transfer` from the deposit address to the target, or the
  value differs from the planned amount) is moved to `needs_review` and gets **no** `deposit_swept`; evmhot logs
  `EVMHOT_ALERT kind=sweep_review network=<n> deposit=<id> tx=<hash>` and an operator decides
  (`POST /admin/retry_sweeps` moves it back to `detected`). Other review kinds: `balance_shortfall`, `no_sender`.
- `(id, event)` stays the receiver's idempotency key.

## Lazy faucet

Registration does **not** fund addresses. Gas is funded just-in-time by the sweeper when a deposit needs sweeping. No `faucet_funding` webhook is emitted at registration time.

## Delivery and retries

Monitor and sweeper **enqueue** webhooks only, in the same database transaction as the state change they report (a
stored deposit or swept mark always has its webhook row); a background worker performs HTTP delivery with configurable retries.

| Status | Meaning |
|--------|---------|
| `pending` | Awaiting delivery or scheduled for retry |
| `delivered` | Receiver returned HTTP 2xx |
| `failed` | `WEBHOOK_MAX_RETRIES` attempts exhausted |

- Idempotency is unchanged: `(id, event)` is the primary key; already-`delivered` rows are not re-enqueued.
- Non-2xx HTTP responses (including 503) count as failures and are retried.
- The worker wakes immediately on enqueue and on `POST /admin/retry_webhooks`.
- Duplicate POSTs are prevented via a lease (`WEBHOOK_LEASE_SECONDS`) claimed before each attempt.

Admin retry (requires `WEBHOOK_JWT_TOKEN`):

```bash
curl -X POST http://localhost:3000/admin/retry_webhooks \
  -H "Authorization: Bearer $WEBHOOK_JWT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"id":"base:0xabc:120","event":"deposit_swept"}'
```
