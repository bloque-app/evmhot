# Fix plans

Engineering fix plans for issues found in review. Each doc has problem, root cause with
file:line refs, an opinionated fix (with options where there's a real decision), tests, and
risk-if-not-fixed.

## Priority order

1. [Spam-token sweep guard](plan-spam-token-sweep-guard.md) — main plan, active Alchemy
   credit drain. Ship first; set `ALLOWED_TOKEN_ADDRESSES` before deploying.
2. [P0: register address collisions](fix-p0-register-address-collision.md) — silent money
   misattribution / unrecoverable funds at scale.
3. [P0: verify reports unmined tx as Success](fix-p0-verify-unmined-tx.md) — payments
   "verified" before they finalize.
4. [P1: verify buffer-overrun panic](fix-p1-verify-buffer-overrun-panic.md) — crafted token
   log crashes `/verify_transfer`.
5. [P1: unauthenticated API](fix-p1-api-authentication.md) — faucet drain + monitor tampering.
6. [P1: faucet nonce race](fix-p1-faucet-nonce-race.md) — funding/sweep failures under load.

## Notes

- The P0 register fix and the P1 API-auth fix compound: closing auth blunts the faucet-drain
  vector, and fixing the index allocator stops account-table pollution from spam registers.
- The P1 buffer-overrun panic shares a decoder with the monitor — fix extracts one shared
  `decode_transfer_amount` helper used by both detection and verification.
- Two unresolved decisions are flagged inline: empty-allowlist posture
  (plan-spam-token-sweep-guard.md) and missing-receipt error shape (fix-p0-verify-unmined-tx.md).
