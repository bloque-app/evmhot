# Fix (P0): Deposit-address collisions in `register`

Severity: P0 — silent money misattribution / unrecoverable funds
Confidence: 8/10
Location: `src/lib.rs:404-416`, `src/db.rs:65-96`

## Problem

`register` derives the wallet index from a hash of the account `id`:

```rust
let mut hasher = DefaultHasher::new();
request.id.hash(&mut hasher);
let hash = hasher.finish();
let index = (hash & 0x7FFFFFFF) as u32;
```

Two distinct defects:

### A. Birthday collisions on a 31-bit index

The index space is `2^31` (~2.1B) and there is **no collision check**. By the birthday
bound, ~46,000 accounts give a ~50% chance that two different `id`s derive the **same
index**, hence the **same deposit address**. `ADDRESS_TO_ID` is last-writer-wins
(`src/db.rs:77-78`), so once two accounts share an address:

- The monitor maps incoming deposits at that address to whichever `id` registered last.
- Deposits intended for account A are attributed (and swept/credited) under account B.

This is silent — no error, no log. For a payment processor, 46k accounts is reachable.

```
id "alice" ─hash─> index 12345 ─> 0xABC...  ┐
                                            ├─ same address, ADDRESS_TO_ID["0xABC"] = last writer
id "bob"   ─hash─> index 12345 ─> 0xABC...  ┘
```

### B. `DefaultHasher` is not stable across Rust versions

`std::collections::hash_map::DefaultHasher` is explicitly documented as **not guaranteed
stable** between Rust releases. The derived index is persisted only indirectly (via the
stored address). If the toolchain changes and the DB is ever rebuilt/replayed, the same
`id` derives a **different** address. Funds already sent to the old address become
unreachable (the service no longer derives that index for that `id`).

## Fix

Pick one of two approaches.

### Option 1 (recommended): sequential index + persisted counter

- Store a monotonic `next_index` counter in the `STATE` table.
- On `register`, allocate `index = next_index`, increment, and persist atomically in the
  same write transaction that inserts the account.
- No hashing, no collisions, stable forever. `get_next_derivation_index` already exists
  (`src/db.rs:51-63`) but is O(N) and unused — replace it with the counter.

```rust
// db.rs: allocate-and-persist in one write txn
pub fn allocate_next_index(&self) -> Result<u32> {
    let write_txn = self.db.begin_write()?;
    let next = {
        let mut state = write_txn.open_table(STATE)?;
        let cur: u32 = state.get("next_index")?
            .map(|v| v.value().parse().unwrap_or(0)).unwrap_or(0);
        state.insert("next_index", (cur + 1).to_string().as_str())?;
        cur
    };
    write_txn.commit()?;
    Ok(next)
}
```

### Option 2: stable keyed hash + collision loop

- Replace `DefaultHasher` with a fixed algorithm (e.g. SHA-256 of `id`, take low 31 bits).
- After deriving the address, check `ADDRESS_TO_ID`; on collision, re-hash with a salt/counter
  until a free index is found. Persist the chosen index alongside the account.

Option 1 is simpler, fully deterministic, and removes the collision class entirely. Option 2
keeps id-derived indices (useful only if you need to re-derive without DB state, which this
service does not — it always checks `get_account_by_id` first at `src/lib.rs:391`).

## Migration / compatibility

- Existing accounts already have a stored `index` and `address`; leave them untouched.
- Initialize the `next_index` counter above the current max existing index (one-time scan)
  so new sequential allocations never collide with legacy hash-derived ones.

## Tests

- Registering N accounts yields N **distinct** indices and addresses.
- Re-registering an existing `id` returns the existing address (no new index) — preserves
  current behavior at `src/lib.rs:391-402`.
- Forced-collision test (Option 2) or counter-monotonicity test (Option 1).
- Counter persists across `Db` reopen (write, drop, reopen, allocate -> no reuse).

## Risk if not fixed

Direct, silent loss or misattribution of customer funds at scale. Highest-priority bug in
the codebase.
