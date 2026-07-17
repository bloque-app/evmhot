# TODOS

## Guard the `next_index` counter before it crosses BIP44's 2^31 hardened-derivation boundary

- **What:** Add a hard ceiling check (reject registration with a clear error) and an alert threshold on the `next_index` derivation counter.
- **Why:** Production `next_index` was observed at ~2,147,441,364 — roughly 42k registrations away from 2^31 (2,147,483,648). At that boundary BIP44 index semantics change (hardened derivation bit), and blindly incrementing past it would derive addresses from an unintended key path or fail, depending on the wallet library's handling.
- **Pros:** Converts a silent future key-derivation bug in a wallet service into a loud, actionable error plus advance warning.
- **Cons:** None meaningful; a range check on one counter. The real work is deciding the remediation (index recycling, second account gap, or new xpub branch) before the ceiling is hit.
- **Context (July 2026):** The counter lives in the `state` table (SQLite today, `UPDATE ... RETURNING` on Postgres after the storage migration). It only ever increments — `register_account_auto` allocates it atomically per registration. Discovered during the Postgres migration eng review (plan `evmhot_postgres_storage_migration`). Start in `register_account_auto` in the storage backend(s) and add a CloudWatch-visible warning log at, e.g., 2^31 minus 100k.
- **Depends on / blocked by:** Nothing. Orthogonal to the Postgres migration, but touching the same function — cheapest to do right after that migration lands.

## Measure Monitor catch-up write throughput on Postgres; add batching only if slow

- **What:** After the Postgres cutover, measure how long Monitor catch-up takes when replaying a large block backlog (deposits recorded one statement at a time), then decide whether to wrap the per-chunk deposit writes in a single transaction.
- **Why:** The SQLite writer actor batched up to 50 background writes per transaction to amortize EFS fsyncs. The Postgres backend deliberately ships without batching (eng-review decision D10, measure-first) because each in-VPC round-trip is ~1-2ms and no user waits on catch-up. The unverified assumption: a 10k-deposit catch-up costing 10-20s is acceptable.
- **Pros:** Either closes the question with data (no work needed) or justifies a small, targeted fix (one transaction around Monitor's chunk loop) instead of speculative machinery.
- **Cons:** Requires remembering to actually look — a long-downtime catch-up event is the natural trigger.
- **Context (July 2026):** Batching removal is intentional; `postgres.rs` has no writer actor or lanes. The measurement is reading CloudWatch timing on Monitor catch-up logs after any extended downtime. If slow, the fix belongs in Monitor's catch-up chunk loop, not in a resurrected writer actor.
- **Depends on / blocked by:** Postgres migration landed and cut over (plan `evmhot_postgres_storage_migration`).
