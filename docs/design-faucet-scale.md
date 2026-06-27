# Design: High-throughput faucet scaling (deferred)

This document captures the scaling path for faucet funding when registration and sweep volume exceeds what a single EOA can sustain. It is **not implemented** in the P1 nonce-race fix; that fix builds the wallet-bound provider once, reuses alloy's `NonceFiller`, shares one `Faucet` instance, and resets the nonce cache on send failure.

## Current ceiling (single faucet EOA)

- One faucet account (`faucet_mnemonic` index 0) sends every existential-deposit funding tx.
- Nonces are strictly ordered; nodes cap pending txs per account (geth `txpool.accountslots`, typically ~16).
- With a reused provider + `NonceFiller`, throughput is bounded by RPC latency and chain inclusion, not by per-call nonce races.
- Practical upper bound for one EOA: on the order of tens to low hundreds of funding txs/sec under ideal conditions, not thousands/sec.

Per-user derived deposit addresses do **not** share this bottleneck: each has its own nonce sequence. The sweeper also processes deposits sequentially today.

## Target: thousands of funding ops/sec

Requires one or more of:

### 1. Faucet account pool

- Derive N faucet signers from the same mnemonic (indices 0..N-1).
- Round-robin or least-loaded assignment when funding an address.
- Each signer has an independent nonce lane → N× parallel in-flight funding txs (still capped per account by mempool limits).
- Operational requirements:
  - Monitor and top up balances across all pool accounts.
  - Persist assignment or idempotency if a fund is retried after partial failure.
  - One `Faucet` (or pool manager) holding N wallet-bound providers, each with its own `NonceFiller`.

### 2. Batched funding (disperse / multicall)

- Single tx funds K deposit addresses (native transfer batch or disperse contract).
- Divides on-chain tx count by K; best lever for burst registration.
- Requires:
  - Deployed disperse (or similar) contract owned/trusted by the service.
  - Batch sizing policy (gas limit, K max).
  - Failure semantics: partial batch failure vs all-or-nothing.

### 3. Sweeper loop concurrency

- `process_deposits` is sequential; at high deposit volume, sweep latency grows independently of faucet throughput.
- Options: worker pool with per-address locking, or queue + dedicated sweep workers.
- Faucet funding from the sweeper remains a call into the shared pool/batch layer above.

## Recommended sequencing

1. **Done (P1):** Single shared `Faucet`, provider built once, `NonceFiller` reuse, reset on send failure.
2. **Next when sustained load > ~10–20 funds/sec:** Faucet account pool (N=4–16), minimal API change at call sites.
3. **When registration bursts dominate:** Batch disperse for register path; keep pool for sweeper top-ups.
4. **When sweep backlog grows:** Parallelize sweeper with per-address serialization only.

## Metrics to watch before building

- Faucet funding error rate (`nonce too low`, `replacement`, RPC errors).
- Time from register/sweep-trigger to funded balance on chain.
- Mempool depth / pending count for faucet address(es).
- Sweeper queue depth and age of oldest unswept deposit.

## Out of scope for this design doc

- Alloy upgrade off 0.1.4.
- API authentication / rate limits on `POST /register` (see `docs/fix-p1-api-authentication.md`).
