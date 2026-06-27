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
  "amount": "1000000",
  "token_type": "erc20",
  "token_symbol": "USDT",
  "token_address": "0xc2132d05d31c914a87c6611c10748aeb04b58e8f",
  "token_decimals": 6
}
```

## deposit_swept

Same fields as detection, plus for ERC20:

```json
{
  "sweep_tx_hash": "0xsweep..."
}
```

## Lazy faucet

Registration does **not** fund addresses. Gas is funded just-in-time by the sweeper when a deposit needs sweeping. No `faucet_funding` webhook is emitted at registration time.
