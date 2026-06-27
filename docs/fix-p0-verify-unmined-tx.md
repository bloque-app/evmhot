# Fix (P0): `verify_native_transfer` reports unmined transactions as Success

Severity: P0 — payments "verified" before they are final
Confidence: 7/10
Location: `src/lib.rs:191-254` (compare with ERC20 path `src/lib.rs:256-284`)

## Problem

In `verify_native_transfer`, the receipt is optional and the revert/status check is only
run when a receipt exists:

```rust
let receipt = self.provider.get_transaction_receipt(tx_hash).await?;
let block_number = receipt.as_ref().and_then(|r| r.block_number);

// Check if transaction was successful
if let Some(ref r) = receipt {
    if !r.status() {
        return Ok(VerifyTransferResponse::Error { /* reverted */ });
    }
}

// ... proceeds to compare tx.to / tx.value regardless of receipt presence
```

If `get_transaction_receipt` returns `None` (transaction is in the mempool but **not yet
mined**), the `if let Some` block is skipped entirely. Execution falls through to compare
`tx.to` and `tx.value` (which are available from the pending tx) and returns
`VerifyTransferResponse::Success` with `block_number: None`.

Consequences:

- A transaction that is still pending — and may later be **dropped, replaced (RBF), or
  reverted** — is reported as a verified, successful payment.
- The ERC20 path does NOT have this bug: it errors when the receipt is missing
  (`src/lib.rs:269-273`). The native path is inconsistent.

```
get_transaction_by_hash  -> Some(pending tx)      // exists in mempool
get_transaction_receipt  -> None                  // not mined yet
                         -> status check SKIPPED
                         -> to/value match         // from mempool data
                         -> Success (block_number: None)   <-- WRONG
```

## Fix

Require a receipt with a successful status (and, ideally, a minimum confirmation depth)
before returning Success. Mirror the ERC20 path.

```rust
let receipt = self
    .provider
    .get_transaction_receipt(tx_hash)
    .await?
    .ok_or_else(|| anyhow::anyhow!("Transaction not yet mined"))?; // or return Error variant

if !receipt.status() {
    return Ok(VerifyTransferResponse::Error {
        message: "Transaction failed (reverted)".to_string(),
        token_type: Some("native".to_string()),
        block_number: receipt.block_number,
    });
}
```

Decisions to confirm:

- **Missing receipt -> `Err` vs `Error` variant.** Returning the structured
  `VerifyTransferResponse::Error { message: "not yet mined" }` is friendlier to callers than
  an HTTP 500; the ERC20 path currently uses `Err` (HTTP 500). Pick one and make both paths
  consistent.
- **Confirmation depth.** Optionally require `current_block - receipt.block_number >= N`
  (reuse `block_offset_from_head` from config) so verification matches the monitor's
  confirmation policy and resists reorgs.

## Tests

- Mined + success -> `Success` with `block_number: Some`.
- Mined + reverted (`status() == false`) -> `Error` "reverted".
- No receipt (pending) -> NOT `Success` (either `Err` or `Error` "not yet mined").
- (If depth added) mined but below confirmation depth -> NOT `Success`.

Use the existing wiremock harness in `src/tests.rs` to stub `eth_getTransactionByHash` /
`eth_getTransactionReceipt`.

## Risk if not fixed

Downstream systems credit a user/order based on a "verified" payment that never finalizes,
enabling double-spend / dropped-tx fraud on the native-currency path.
