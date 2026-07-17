# Fix (P1): Panic in `verify_erc20_transfer` on Transfer logs with >32 bytes of data

Severity: P1 — request-handler panic (DoS / crash)
Confidence: 7/10
Location: `src/lib.rs:324-329` (safe reference impl: `src/monitor.rs:196-208`)

## Problem

```rust
// src/lib.rs
let amount = if !log.data().data.is_empty() {
    U256::from_be_slice(&log.data().data)
} else {
    U256::ZERO
};
```

`U256::from_be_slice` **panics if the slice is longer than 32 bytes** (ruint contract). A
standard ERC20 `Transfer` carries exactly 32 bytes in `data`, but a non-standard or
malicious token can emit a `Transfer(address,address,uint256)` log with extra trailing data.
When `/verify_transfer` processes such a log, the handler panics.

The monitor already handles this correctly by taking the first 32 bytes:

```rust
// src/monitor.rs
let amount = if log.data().data.len() >= 32 {
    let amount_bytes: [u8; 32] = log.data().data[..32].try_into().expect("slice length is 32");
    U256::from_be_bytes(amount_bytes)
} else if !log.data().data.is_empty() {
    U256::from_be_slice(&log.data().data)
} else {
    U256::ZERO
};
```

So the codebase has both a safe and an unsafe decoder for the same value — the verify path
uses the unsafe one.

## Fix

Make the verify path use the same bounded decode as the monitor. Extract a single shared
helper to remove the duplication (DRY) and guarantee both paths behave identically.

```rust
/// Decode an ERC20 Transfer `value` from log data, tolerant of non-standard tokens
/// that pad or append extra bytes. Never panics.
pub fn decode_transfer_amount(data: &[u8]) -> U256 {
    if data.len() >= 32 {
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&data[..32]);
        U256::from_be_bytes(buf)
    } else if !data.is_empty() {
        U256::from_be_slice(data)
    } else {
        U256::ZERO
    }
}
```

Call it from both `src/lib.rs` (verify) and `src/monitor.rs` (detection).

## Tests

- `decode_transfer_amount`: empty -> 0; <32 bytes -> right-aligned value; exactly 32 bytes
  -> exact value; **>32 bytes -> first 32 decoded, no panic** (the regression case).
- Verify-path test: receipt log with a 64-byte `data` field does not panic and decodes the
  leading 32 bytes.

## Risk if not fixed

A single crafted token transaction passed to `/verify_transfer` crashes the request task.
Combined with the unauthenticated API (see `fix-p1-api-authentication.md`), this is a remote
crash vector.
