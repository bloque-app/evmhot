# Fix (P1): Faucet nonce race under concurrency

Severity: P1 — funding/sweep failures under load
Confidence: 7/10
Location: `src/faucet.rs:36-85`, callers `src/lib.rs:430` (register) and `src/sweeper.rs:264,425` (sweeper)

## Problem

All faucet funding uses the single faucet signer at index 0:

```rust
let signer = self.wallet.get_signer(0)?;
let faucet_provider = ProviderBuilder::new()
    .with_recommended_fillers()   // fetches nonce per-tx via eth_getTransactionCount(pending)
    .wallet(wallet)
    .on_provider(&self.provider);
let pending_tx = faucet_provider.send_transaction(tx).await?;
```

Multiple callers invoke `fund_new_address` **concurrently**:

- `register` spawns a fire-and-forget funding task per registration (`src/lib.rs:430`).
- The sweeper funds addresses from its loop when balances are too low for gas
  (`src/sweeper.rs:264` for native, `src/sweeper.rs:425` for ERC20).

`with_recommended_fillers` resolves the nonce independently for each transaction by querying
`eth_getTransactionCount(..., pending)`. When two funding transactions are built before
either is mined, both read the **same** nonce. The chain accepts one; the other fails with
`nonce too low` / `already known` / replacement errors.

```
t0: register("a") ─ getTransactionCount(pending) = 7 ─ send nonce=7 ┐
t0: sweeper fund   ─ getTransactionCount(pending) = 7 ─ send nonce=7 ┘  one of these is rejected
```

Net effect under load: intermittent funding failures, which cascade into failed sweeps
(addresses never get gas) — exactly when volume is highest.

## Fix

Serialize faucet sends so nonces are assigned in order.

### Option A (recommended): mutex around faucet sends

- Wrap the send section in an `async` mutex held by the `Faucet` (e.g. `tokio::sync::Mutex<()>`).
  Acquire before building/sending, release after `send_transaction` returns the pending tx
  (you can release before waiting for the receipt, since the nonce is consumed at submission).
- Minimal change, removes the race. Throughput is bounded by submission latency, acceptable
  for a faucet.

```rust
pub struct Faucet<P> {
    wallet: Wallet,
    provider: P,
    existential_deposit: U256,
    send_lock: tokio::sync::Mutex<()>,
}
// in fund_new_address:
let _guard = self.send_lock.lock().await;
// build + send_transaction under the guard
```

### Option B: explicit nonce manager

- Track the faucet nonce in the `Faucet` (seed from `getTransactionCount` once, then
  increment locally), assign explicitly with `.with_nonce(n)`. More robust at high
  throughput but needs reconciliation on restart and on send failure (gap handling).

Option A is the right-sized fix for current volume; revisit Option B only if the faucet
becomes a throughput bottleneck.

## Tests

- Two concurrent `fund_new_address` calls both succeed (serialized), each with a distinct
  nonce — verify against a wiremock that records submitted nonces, or an integration test on
  a local node (anvil).
- Faucet insufficient-balance path still returns the existing error (`src/faucet.rs:53-61`).

## Risk if not fixed

Under registration/sweep bursts, funding txs collide and fail, leaving deposit addresses
without gas so their sweeps never complete — funds sit unswept until manual intervention.
