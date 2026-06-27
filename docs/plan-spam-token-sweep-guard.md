# Plan: Spam-token sweep guard (stop the Alchemy credit drain)

Status: ready to implement
Priority: P0 (active credit burn)
Scope: `src/config.rs`, `src/main.rs`, `src/monitor.rs`, `src/sweeper.rs`, `src/tests.rs`, `src/e2e_tests.rs`, `.env`, `README.md`

## Problem

The sweeper logs a flood of:

```
ERROR evm_hot_wallet::sweeper: Failed to sweep ERC20 deposit 0x..:NNN: buffer overrun while deserializing
```

every poll cycle, and each attempt spends Alchemy compute units.

## Root cause

`buffer overrun while deserializing` is alloy's ABI-decode failure on the `balanceOf`
call at the start of `sweep_erc20_deposit` (`src/sweeper.rs:686-688`):

```rust
let contract = IERC20::new(token_address, provider);
let balance = contract.balanceOf(owner_address).call().await?._0;
Ok(balance)
```

The error appears one RPC round-trip after the `Signer address:` log line, confirming it
is the `balanceOf` call, not the transaction send.

The deposits are **spam tokens**: the symbol `USDC` shows up at 10+ different contract
addresses in the logs (and `USDT0` at several more). Legit stablecoins have one canonical
address per chain. These fake tokens emit counterfeit `Transfer(address,address,uint256)`
events to get picked up by indexers. Their `balanceOf` returns empty/garbage data, so
decoding into `uint256` fails (one even returns `execution reverted`).

How they enter the system: the monitor records a deposit for **any** token whose `Transfer`
event targets a monitored address, as long as the symbol is <= 5 chars
(`src/monitor.rs:193-246`). There is no token allowlist. The sweeper then calls `balanceOf`
on each, every cycle, forever — and every new block brings fresh spam.

```
Block ──> monitor.process_erc20_transfers ──(any token, symbol<=5)──> ERC20_DEPOSITS(detected)
                                                                              │
                              sweeper.process_deposits ──balanceOf RPC──> buffer overrun ──> retry next cycle
```

## What already exists

- Infinite-retry guard: `MAX_SWEEP_RETRIES = 5` (`src/sweeper.rs:65`), `SWEEP_FAILURES`
  table, `increment_sweep_failure_count`, `mark_erc20_deposits_failed_for_account_token`,
  `mark_erc20_deposit_failed` (`src/db.rs:418-514`). Committed 2026-04-04 (`bcd4a3e`), on
  `main`. It caps retries at 5 per deposit but still costs 5 calls each and does nothing
  about the steady influx of new spam.
- Crude symbol-length filter in the monitor (`src/monitor.rs:219`).

So if the running binary still shows endless retries, **confirm the deployed commit** — it
may predate `bcd4a3e`.

## Fix

### 1. Env-configured allowlist on `Config` (`src/config.rs`)

- Add `pub allowed_token_addresses: std::collections::HashSet<String>` (normalized lowercase).
- In `from_env`, parse `ALLOWED_TOKEN_ADDRESSES` (comma-separated): trim, lowercase, drop
  empties. Normalize so a bare and a `0x`-prefixed address compare equal (addresses from
  `Address::to_string()` always carry the `0x` prefix).
- Add `pub fn is_token_allowed(&self, token_address: &str) -> bool` that returns `true`
  when the set is empty (allowlist disabled) or contains the normalized address.

**Empty-set semantics (UNRESOLVED decision — defaulting to allow-all):** empty/unset =
allowlist disabled = allow all, for safe rollout. Consequence: the drain only fully stops
once `ALLOWED_TOKEN_ADDRESSES` is populated; until then only fail-fast (5 -> 1 calls per
spam token) mitigates it. **Deploy order: set the env var, then ship.** Log a loud `WARN`
at startup (`src/main.rs`) when the set is empty so the inert state is obvious.

Alternative postures if you want a hard guarantee on deploy: empty = block all sweeps, or
empty = fatal startup error. Both risk halting real fund movement on a misconfigured deploy.

### 2. Filter spam at detection (`src/monitor.rs`)

In `process_erc20_transfers`, after confirming `to_address` is monitored and **before**
`get_or_fetch_token_metadata` (which itself makes 3 RPC calls), `continue` if
`!self.config.is_token_allowed(&token_address.to_string())`. Log at `debug` to avoid log
spam. Prevents spam from ever entering the DB and saves the symbol/decimals/name RPC calls.

### 3. Skip + fail-fast in the sweeper (`src/sweeper.rs`)

- At the top of the per-deposit loop in `process_deposits` (around line 137), if
  `!self.config.is_token_allowed(&deposit.token_address)`, mark just that deposit failed via
  `self.db.mark_erc20_deposit_failed(&deposit.key)` and `continue` — **no RPC call**. Because
  the loop visits every detected deposit in one cycle, all currently-stored spam is cleared
  to `failed` in a single pass at zero Alchemy cost.
- Extract a pure classifier helper (not an inline string match):

  ```rust
  /// Deterministic, non-retryable sweep errors (junk / non-ERC20 token contracts).
  /// Matches stable alloy decode-failure substrings. Unit-tested so an alloy upgrade
  /// that changes the wording fails a test instead of silently re-enabling retries.
  fn is_permanent_sweep_error(err_debug: &str) -> bool {
      let s = err_debug.to_ascii_lowercase();
      s.contains("buffer overrun") || s.contains("deserializ")
  }
  ```

- In the `Err(e)` branch (lines 195-216), if `is_permanent_sweep_error(&format!("{:?}", e))`,
  mark the account+token failed immediately (1 strike) via
  `mark_erc20_deposits_failed_for_account_token`. Keep the existing 5-retry path for other
  (potentially transient) errors like `execution reverted`.

### 4. Update remaining `Config` literals

Add `allowed_token_addresses: Default::default(),` (empty = disabled) to the 7 literals in
`src/tests.rs` and the 1 in `src/e2e_tests.rs`.

### 5. Docs / env

- Add a commented `ALLOWED_TOKEN_ADDRESSES=` entry to `.env` with the format and the
  deploy-order note (set before shipping).
- Note the variable in `README.md`.

## Tests (harness already has wiremock + tempfile)

- `Config::is_token_allowed`: empty -> allows all; present (case-insensitive, with/without
  `0x`) -> true; absent -> false.
- Allowlist parsing: comma list, surrounding whitespace, mixed case, empty entries dropped.
- `is_permanent_sweep_error`: `"...buffer overrun while deserializing"` -> true;
  `"execution reverted"` and generic errors -> false.
- Monitor skip (highest value): a `Transfer` to a monitored address for a non-allowlisted
  token is NOT recorded (and triggers no metadata RPC); an allowlisted one IS recorded.
- Sweeper skip (DB-level): a `detected` deposit whose token is not allowlisted is marked
  `failed` and leaves the `detected` set, with no provider call.

## Result

- Once `ALLOWED_TOKEN_ADDRESSES` is set, spam tokens are skipped with **zero** RPC calls in
  both monitor and sweeper — the credit drain is eliminated.
- Even with the allowlist off, a decode error now permanently marks the deposit failed after
  1 attempt instead of retrying forever.
- No DB migration: existing spam `detected` rows are marked `failed` on the next sweep cycle
  without any RPC.

## NOT in scope (deferred)

- Dedupe the duplicated `IERC20` `sol!` block across `monitor.rs`/`sweeper.rs`.
- Pruning/compaction of old `failed`/`swept` rows in `ERC20_DEPOSITS`.
- Fast-failing `execution reverted` (kept on the 5-retry path; reverts can be transient).
