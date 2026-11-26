# evmhot — System Specification

**Version:** 0.2.8 · **Status:** draft for review · **Owner:** @pandres95

## 0. How to read this document

This document says what evmhot does. `PLAN.md` says how it is built and deployed, and `features/` holds the work.
If code, plan and spec disagree, the spec wins. A change to behaviour starts as an amendment in `amendments/`.

Version 0.1.0 described the service exactly as it ran in production up to 2026-10 (§13 lists where that differs from
this version). Version 0.2.0 applies amendment 0001, the first set of rulings. Requirements that the code does not meet
yet are marked **(not built)**, and `PLAN.md` §4 names the feature that builds each one.

### Change history

| Version | Date | Change |
|---|---|---|
| 0.1.0 | 2026-11-09 | The service as it ran: registration, detection, sweeps, faucet, webhooks, store, API |
| 0.2.8 | 2026-11-26 | Amendment 0008 **(not built)**: the `cctp_burn` target kind (Base USDC burned over CCTP V2 and minted to the Polygon reserve), the sender check before a burn, the relayer key, `deposit_swept` 1.1.0 |
| 0.2.7 | 2026-11-26 | Repair: restores §6.2, §6.3, `REQ-DT-9` and the withdrawn `REQ-DT-10`, which the 0.2.5 edit dropped; 0006's retry rule becomes `REQ-DT-11`; adds `REQ-FC-3` |
| 0.2.6 | 2026-11-23 | Amendment 0007: `faucet_low` when a faucet's balance is below its network's floor |
| 0.2.5 | 2026-11-12 | Amendment 0006: `rpc_urls` pool with failover, backoff on rate limits instead of discarding a chunk, no skipping a block a lagging endpoint reports missing, a faucet top-up that was sent is sent |
| 0.2.4 | 2026-11-09 | Amendment 0005: a second delivery of `deposit_swept` to a target's notify URL; the published `deposit_swept` contract |
| 0.2.3 | 2026-11-09 | Amendment 0004: sweep intents before broadcast, review states, one sweep per transfer, alerts, `sweep_target` |
| 0.2.2 | 2026-11-09 | Amendment 0003: the `hold` target kind; `sweeps` in `deposit_swept` |
| 0.2.1 | 2026-11-09 | Amendment 0002: derivation indices are 32-bit raw child numbers; `register`'s response shape; JSON error bodies |
| 0.2.0 | 2026-11-09 | Amendment 0001: sweep target kinds and bounds, the sweep coordinate, finality per network, sweep outcome check, lag and provider alerts, atomic outbox, separate tokens, counter ceiling; withdraws the symbol filter |

### Normative language

MUST, MUST NOT, SHOULD and MAY are used as in RFC 2119, in normative text only.

### Identifiers

| Family | Meaning |
|---|---|
| `REQ-<AREA>-<n>` | A requirement. Areas: `RG` registration, `KY` keys and addresses, `DT` detection, `SW` sweeps, `FC` faucet, `WH` webhooks, `API` the HTTP surface, `ST` store, `OB` observability, `CF` configuration |
| `INV-<n>` | An invariant: true at every moment, not only after an operation |
| `AC-<n>` | An acceptance check a test names |
| `OQ-<n>` | An open question. Never resolved by implementing one of its options |

Identifiers are never renumbered. A withdrawn identifier stays listed as withdrawn.

## 1. Purpose and scope

### 1.1 What evmhot is

evmhot gives each registered party a deposit address on EVM networks, watches those addresses for deposits of
admitted tokens, moves each deposit's funds to a configured sweep target, and tells the registering service what it
saw and what it moved. One address serves every configured network. The service holds the keys of the deposit
addresses (a hot wallet) and of a faucet that pays the sweeps' gas.

evmhot comes in two forms with the same behaviour: a **library** that another program embeds, and a
**self-contained service** that runs on its own, with its own HTTP surface, store, configuration and deployment. This
specification binds both; §9 is the service's surface. In 0.1.0 only the library form ran in production, embedded in
a host service.

### 1.2 In scope

- Registration: a stable deposit address per registration id.
- Detection: deposits of admitted tokens (and, where enabled, of the network's native coin) to registered addresses.
- Sweeps: moving the full balance of a detected token from the deposit address to that token's sweep target.
- Gas: funding a deposit address from the faucet just in time for its sweep.
- Notification: a webhook per detected deposit and per swept deposit, delivered at least once.
- Verification: answering whether a given transaction moved a given amount to a given address.
- Running as a self-contained service, with its own store, configuration and deployment, and being embeddable as a
  library.

### 1.3 Out of scope

- What happens to swept funds after they reach the sweep target. A route that turns a sweep into value elsewhere
  belongs to the service that consumes the webhook.
- Attributing a sweep to a party beyond the registration id and the deposit's sender. evmhot pushes no addresses to
  other systems and keeps no list of permitted senders.
- Prices, conversions, fees charged to a party, refunds.
- Custody of the sweep targets.
- Payouts: sending funds from custody to a party, on any network. A `cctp_burn` sweep (§6.7) moves a deposit to its
  target on another network; it pays no one.

## 2. Glossary

| Term | Meaning |
|---|---|
| **network** | One EVM chain evmhot is configured for, named by a short name and its chain id |
| **registration** | The pair (registration id, webhook URL) a client submits, and the address evmhot assigns to it |
| **registration id** | The client's opaque, unique identifier for a party |
| **deposit address** | The address assigned to a registration; the same on every network |
| **derivation index** | The position of a deposit address in the deposit mnemonic's derivation path |
| **admitted token** | A token contract a network's configuration lists; transfers of other tokens are ignored |
| **deposit** | One transfer of an admitted token (one log) or of the native coin (one transaction) into a deposit address |
| **sweep** | One transaction from a deposit address that moves its whole balance of one token to that token's sweep target |
| **sweep target** | Where a token's sweeps go on a network. Kinds are defined in §6.4 |
| **faucet** | The address that pays sweep gas, from a second mnemonic |
| **cursor** | The last block a network's monitor has fully processed |
| **detection height** | The newest block the monitor may process, per §7 |
| **sweep coordinate** | The block number, block hash and block-level log index of the token `Transfer` a sweep emitted into its target |
| **outbox** | The stored queue of webhook deliveries |

## 3. Parties

- **Client.** A service that registers parties and receives webhooks. It is trusted to send unique registration
  ids; it is not trusted with keys.
- **Verifier.** A service that asks whether a transaction moved funds to an address.
- **Operator.** A person who sets configuration, retries a stuck sweep or webhook, and moves a cursor.
- **Network provider.** The RPC endpoint per network. Trusted for reads only up to the finality rule in §7.

## 4. Architecture

One process. For each configured network it runs a **monitor** and a **sweeper**. Shared across networks are the
**faucet** client, the **webhook worker** (which delivers the outbox), the **store** and the **HTTP API**.

- The monitor reads blocks and logs, records deposits and enqueues `deposit_detected`.
- The sweeper takes recorded deposits, funds gas from the faucet, sends sweeps, records their outcome and enqueues
  `deposit_swept`.
- The monitor and the sweeper never call a client directly; only the webhook worker does.
- Keys come from two mnemonics held only in the process's memory (§5.1).

## 5. Keys and addresses

### 5.1 Keys

- `REQ-KY-1` Deposit addresses MUST be derived from one deposit mnemonic by the standard EVM derivation path,
  `m/44'/60'/0'/0/<derivation index>`, the index taken as a raw 32-bit child number: an index at or above 2^31 is a
  hardened child at that number, as in 0.1.0. Existing addresses use both ranges.
- `REQ-KY-2` The faucet MUST use a separate mnemonic.
- `REQ-KY-3` Neither mnemonic, nor any key derived from them, MAY be logged, returned by the API, written to the
  store, or included in a webhook.
- `REQ-KY-4` A change of deposit mnemonic strands every existing address. The deposit mnemonic MUST NOT change while
  any registration exists.
- `REQ-KY-5` **(not built)** The **relayer**, which relays `cctp_burn` mints (`REQ-SW-18`), is the faucet mnemonic's
  address at index 1. It pays only for relays, and the faucet tops it up on the destination (`REQ-FC-1`). Exactly one
  process may use a relayer key, so environments MUST use different faucet mnemonics. (Amendment 0008.)

### 5.2 Registration

- `REQ-RG-1` `register(id, webhook_url)` returns the deposit address for `id`. A repeated call with the same `id`
  returns the same address and changes nothing.
- `REQ-RG-2` A new registration takes the next derivation index from a stored counter, allocated atomically with
  the registration, so two registrations never share an index.
- `REQ-RG-3` The counter MUST start above every derivation index already in the store. Indices are not contiguous:
  older registrations used hashed indices, some at or above 2^31 (`REQ-KY-1`).
- `REQ-RG-4` Registration funds nothing. Gas is funded at sweep time (§6.3).
- `REQ-RG-5` Derivation indices are 32-bit. When the counter reaches 2^32, registration MUST be refused
  with an error; it MUST NOT wrap or overflow. (Revised by amendment 0002.)

## 6. Deposits and sweeps

### 6.1 Detection

- `REQ-DT-1` For each network, the monitor processes blocks in order from the cursor to the detection height, and
  advances the cursor only after a block's deposits are stored.
- `REQ-DT-2` A token deposit is a `Transfer` log of an admitted token whose recipient is a deposit address and whose
  sender is not the faucet. Its identity is `(network, transaction hash, block-level log index)`.
- `REQ-DT-3` A native deposit is a transaction with value whose recipient is a deposit address and whose sender is
  not the faucet. Its identity is `(network, transaction hash)`.
- `REQ-DT-4` A deposit below the configured minimum for its token is skipped: it is not stored, not notified and not
  swept.
- `REQ-DT-5` More than 10 blocks behind the detection height, the monitor processes ranges of blocks with one ranged
  log query per range, splitting a range the provider refuses, while still fetching every block for native
  deposits. The outcome MUST equal block-by-block processing.
- `REQ-DT-6` Processing a block twice records nothing new (`INV-2`).
- `REQ-DT-9` A network with no stored cursor starts at the current detection height. Deposits made before that are
  not detected.
- `REQ-DT-10` **Withdrawn by amendment 0001.** In 0.1.0 a token transfer whose symbol was longer than five characters
  was dropped before it was stored: no deposit, no webhook. A token whose `symbol()` call failed was named `UNKNOWN`
  and dropped the same way. Admission is by contract address only (`REQ-DT-2`); the symbol is metadata.
- `REQ-DT-11` A block fetch or `eth_getLogs` that fails with a 429, 401-404, 413, 5xx or transport error is retried with
  a wait of 0.5 s doubling to 8 s (6 attempts) before the chunk fails. A `null` block at or below the head is an
  error, retried the same way: the cursor never passes a block that was not read. A range is bisected for a range or
  size error once every endpoint has refused it, and for a 5xx or timeout once the backoff is spent, never for a 429. A lagging endpoint may still answer
  empty logs for blocks it lacks; the offset floor makes that unlikely, and F-002b's two-node agreement answers it.
  (Amendment 0006, which named it `REQ-DT-9`, an identifier already in use.)

### 6.2 Sweeps

- `REQ-SW-1` In 0.1.0 a sweep moves the deposit address's **whole** balance of the token, not the deposit's amount.
  Several deposits of one token to one address can therefore be settled by one sweep; each of them is then marked swept
  by that sweep. `REQ-SW-12` overrides this for targets that need a refund address.
- `REQ-SW-2` A sweep goes to the token's sweep target on that network. A token with no target of its own uses the
  network's default target. The native coin always uses the default target. **(not built, amendment 0008)** A deposit whose registration
  is not among a `cctp_burn` target's clients uses the token's other target on that network (`REQ-SW-19`).
- `REQ-SW-3` A token deposit whose address holds a zero balance of the token is retried for 10 passes and then marked
  swept with no sweep transaction. Native deposits have no such rule. It applies only to a group with no recorded sweep
  and no intent other than a failed, dropped or closed-by-operator one; when it settles a deposit whose keys have any
  intent history it raises `settled_without_sweep` (amendment 0004). **(not built, amendment 0008)** It never applies to a deposit with a
  `cctp_burn` part in flight (`REQ-SW-16`).
- `REQ-SW-4` A sweep that fails with a funding error waits for the faucet and is retried. A token sweep that fails
  otherwise counts the failure; after 5 consecutive failures, or at once on a decoding error, the deposit is recorded as
  failed and can be retried by an operator. A native sweep estimates its gas; a failed estimate or a revert counts toward the same 5 failures, and the
  fifth sends the native deposits to review (`native_sweep_failed`) (amendment 0004).
- `REQ-SW-11` A native sweep sends the balance minus the cost of 21,000 gas at the current maximum fee plus 10%. A
  balance at or below that cost is not swept.
- `REQ-SW-5` A deposit MUST be marked swept only after its sweep's receipt shows success. A reverted
  sweep is recorded as failed.

### 6.3 Gas

- `REQ-FC-1` Before a sweep, the sweeper checks the deposit address's native balance against the gas the sweep needs,
  and asks the faucet to top it up if short. **(not built, amendment 0008)** The faucet funds the relayer (`REQ-KY-5`) the same way.
- `REQ-FC-2` A faucet with too little balance makes sweeps wait; it never marks a deposit swept.
- `REQ-FC-3` A faucet top-up counts as sent once it is broadcast. The faucet records each one per address. While
  it is recent (2 minutes once confirmed; 10 minutes while unconfirmed or after "already known"; 90 s after "nonce too low"), a
  request to fund that address is answered "faucet top-up pending" and sends nothing, and the sweep waits for a later
  pass. (Amendment 0006, A3.)

### 6.4 Sweep targets (amendment 0001)

- `REQ-SW-6` A sweep target has a kind:
  - `transfer`: an address. The sweep is a token transfer to it. This is the only kind built today.
  - `directed_deposit` **(not built)**: a contract and a 32-byte recipient. The sweep calls the contract's deposit
    function with the amount and the recipient. The recipient is configuration, one per network and token, and is
    the same for every deposit address.
  - `hold`: no sweep. Deposits are detected, stored and notified with `deposit_detected`, and stay in
    their deposit addresses until the token's target changes to another kind. Changing it releases every held
    deposit to the new target. (Amendment 0003.)
  - `cctp_burn` **(not built)**: a burn through Circle's CCTP V2 that mints the token to a recipient on another
    configured network, the **destination** (§6.7). The mint is the transfer into the target for `REQ-SW-9`, and its
    coordinate is the one in `REQ-SW-10`, on the destination. A `cctp_burn` target MUST be `per_transfer`
    (`REQ-SW-15`) and declare a maximum. Only standard transfers with a zero maximum fee are built. (Amendment 0008.)
- `REQ-SW-7` A target MAY declare a minimum and a maximum per sweep, in the token's minor units:
  - A balance below the minimum MUST NOT be swept to that target. It stays in the deposit address until later
    deposits bring it to the minimum.
  - A balance above the maximum MUST be swept in several sweeps, each at most the maximum. An address sends one part per
    pass (`REQ-SW-13`) and the balance is read again each pass. When the last part would be below the minimum, the
    part before it is cut so the tail is exactly the minimum, if both fit; otherwise the tail stays owed as dust
    (amendment 0004).
- `REQ-SW-8` A target MAY declare that it admits only one token. Configuration that routes any other
  token to it MUST be refused at load time, and the process MUST NOT start.
- `REQ-SW-9` Each sweep into a target emits exactly one token `Transfer` into it.

- `REQ-SW-12` A target MAY declare that each sweep needs one refund address. For such a target, a
  sweep MUST NOT combine deposits from different senders: each sweep covers one inbound transfer, or several from the
  same sender. Deposits from one sender below the target's minimum MAY accumulate until together they reach it.
- `REQ-SW-15` A target MAY declare `per_transfer`: each inbound deposit is swept on its own, never with another, even
  from the same sender. A sweep sends what that deposit is still owed, never capped to the balance (`REQ-SW-14`). A
  deposit above the target's maximum is split evenly into parts of `ceil(remaining / ceil(remaining / max))`, all for
  that deposit. A `per_transfer` target MUST be refused at load if its minimum is above the token's detection minimum,
  if `2 * min > max + 1` (a part could fall below the minimum), or if it also declares `per_sender`. bezent's Safe is a
  `per_transfer` target (R13). (Amendment 0004.) **(not built, amendment 0008)** A `cctp_burn` part in flight counts as paid in what a
  deposit is owed (`REQ-SW-16`).

### 6.6 Sweep intents and review (amendment 0004)

- `REQ-SW-13` An address MUST have at most one signed, unresolved sweep transaction at a time, across every token and
  the native coin, because nonces belong to the address; the store refuses a second open intent. Before signing,
  evmhot records a sweep intent (deposits covered, target, part, amount), then signs locally at an explicit nonce,
  stores the nonce, hash and signed bytes, and broadcasts. No receipt is awaited inside a pass. At the start of every
  pass the address's open intent is resolved first. A signed intent is never dropped and keeps its nonce: a receipt for
  any of its hashes settles or fails it; with no receipt and the nonce unused, the newest bytes are broadcast again, and
  an underpriced transaction that waited 10 passes is re-signed at the same nonce with both fee fields raised by at
  least 10% (or to the estimate); once the nonce has been used for 64 blocks with no receipt of ours, the deposits go to review and the
  intent stays watched: for 5,000 blocks from that verdict every pass reads its receipts, each intent on its own, a late receipt records the sweep and releases
  the deposits in one transaction, together with that address and token's `balance_shortfall` deposits
  (`review_self_resolved`, only when the receipt matches), and an operator retry is refused meanwhile. A sweep unmined with its nonce
  free raises `sweep_stuck` once after 30 minutes.
  Only an intent never signed is dropped. **(not built, amendment 0008)** The same rules hold for the relayer's relay intents (`REQ-SW-18`).
- `REQ-SW-14` A deposit enters `needs_review`, and is not swept until an operator releases it, when its sweep's
  receipt holds no matching `Transfer` or a different amount, or its nonce was used by another transaction
  (`sweep_review`); when, for a `REQ-SW-12` or `REQ-SW-15` target, the address holds less than the live deposits owe
  plus what deposits in review or failed on the pair are still owed (`balance_shortfall`; a sweep is never capped); or
  when such a deposit has no recorded sender (`no_sender`). For a whole-balance target, any deposit on the
  (address, token) pair in `needs_review` or `failed` blocks the pair (`pair_blocked`); for `REQ-SW-12` and
  `REQ-SW-15` targets their amounts are reserved instead. **(not built, amendment 0008)** The reservation counts `cctp_burn` parts in
  flight as paid.

### 6.5 The sweep coordinate (amendment 0001)

- `REQ-SW-10` For each sweep evmhot records, and reports in `deposit_swept`, the sweep's
  transaction hash, block number, block hash, the block-level log index of the `Transfer` into the target, the
  amount swept, and the target address (`sweep_target`, amendment 0004). The log index is the position of that log in its block, taken from the receipt, not its position
  within the transaction.

### 6.7 CCTP burns (amendment 0008)

- `REQ-SW-16` **(not built)** A `cctp_burn` target names this network's `TokenMessengerV2`, the destination's
  `MessageTransmitterV2`, the destination and its token, both CCTP domains, the mint recipient, the attestation API's
  URL, the minimum finality threshold (default 2000) and the maximum fee (default 0).
  - **At load:** the destination MUST be configured; the recipient and token MUST be non-zero; the network MUST have at
    least two RPC endpoints; the target MUST list its clients (`REQ-SW-19`). If the destination configures the
    recipient as a target for that token, the `cctp_burn` target MUST admit only the token, and its bounds MUST lie
    within that target's.
  - **Before each burn**, evmhot reads both `localDomain`s, the messenger's minter and burn limit, the destination
    minter's local token for (source domain, token), and the messenger's minimum fee (a revert of that call reads as
    zero). A mismatch, a maximum above the burn limit or a minimum fee above the maximum fee stops burns on the target
    (`cctp_config`) until a read agrees again. A read that fails in transport means no burn this pass.
  - **The burn:** when the on-chain allowance is below the part, an `approve` of the messenger for the largest amount
    (an intent that settles no deposit), then `depositForBurn` with destination caller zero. Both are sweep intents
    under `REQ-SW-13`. A burn's intent records the destination's head before signing.
  - **The check:** a burn passes only if its receipt holds the messenger's `DepositForBurn` with every argument as
    sent, and the token's `Transfer` of the amount from the address to the minter; anything else is `sweep_review`.
    Every receipt of a burn or approval, a revert included, is acted on only at or below the detection height. Until
    then the intent stays open, and a transaction a reorganisation removes is broadcast again.
  - **In flight:** an accepted burn's part is in flight until its mint passes. It counts as paid in what the deposit
    is owed and in the `REQ-SW-14` reservation. A deposit with nothing left to burn and a part in flight is
    **burned**; it is swept only once every part's mint passes.
- `REQ-SW-17` **(not built)** Before a deposit's first burn, evmhot reads its sender's code on two endpoints, and
  stores the verdict with the deposit.
  - Both empty, or both an EIP-7702 delegation designator (`0xef0100` and 20 bytes): the burn may proceed.
  - Both other code: the deposit enters `needs_review` (`contract_sender`).
  - A failed read or a disagreement: no burn this pass. After an hour of that, `sender_unchecked`, then hourly.
- `REQ-SW-18` **(not built)** For each part in flight, evmhot polls the attestation API for the burn's message, no
  sooner than 10 minutes after the burn, then once a minute. A 404 or empty answer is pending; a 429 pauses every
  attestation request for 5 minutes.
  - **The mint:** it passes only if a destination receipt holds the transmitter's `MessageReceived` with the part's
    nonce and source domain. The coordinate is the destination token's `Transfer(0x0 → recipient)` for the burned
    amount, less the fee executed, between the previous `MessageReceived` in that receipt (or its start) and this one.
    It is acted on only at or below the destination's detection height.
  - **Finding it:** evmhot checks the mint transaction the API reports, if any. Otherwise, if `usedNonces` marks the
    nonce used, it searches `MessageReceived` by nonce in bounded ranges from the head recorded on the burn's intent.
    Otherwise the relayer (`REQ-KY-5`) sends `receiveMessage` as a relay intent for that part under `REQ-SW-13`, one
    open relay intent per relayer, oldest ready part first. An intent's part never changes.
  - **Before signing a relay**, evmhot simulates it and signs nothing that would revert. Each simulated or mined revert
    backs that part off (1 minute doubling to 60), and the fifth raises `relay_failing`, then hourly.
  - **Before any relay rebroadcast or re-sign**, evmhot reads `usedNonces` and the message's `expirationBlock`. If the
    nonce is used, the part settles from that mint and the open relay is left to be mined and revert. If the message
    has expired or expires within 100 blocks, evmhot re-attests it (`POST /v2/reattest/{nonce}`) and re-signs at the
    same nonce with both fee fields raised by at least 10%. An `expirationBlock` of 0 never expires. A mined relay
    that reverts fails its intent, never the deposit.
  - **Unminted:** a part with no passed mint 60 minutes after its burn was accepted raises `cctp_unminted`, then
    hourly. An operator's recourse is a relay by hand, which the next pass finds.
- `REQ-SW-19` **(not built)** A `cctp_burn` target lists the registration clients it serves, by the origin of the
  registration's webhook URL. Only deposits of those registrations are swept to it; the others use the token's other
  target on the network (`REQ-SW-2`).

## 7. Finality and the detection height

- `REQ-DT-7` In 0.1.0 the detection height is the network's newest block minus a configured offset (default 20).
- `REQ-DT-8` **(not built)** A network's configuration MUST choose its detection height as one of: the provider's
  `finalized` block, its `safe` block, or the newest block minus an offset. The default is `finalized` where the
  network has it.

## 8. Webhooks

- `REQ-WH-1` Two events: `deposit_detected` and `deposit_swept`. Each is sent to the registration's webhook URL as an
  HTTP POST with a JSON body, with a bearer token if one is configured. In 0.1.0 that token is the same one the admin
  routes accept (`REQ-WH-8`).
- `REQ-WH-2` Every webhook carries an `id` that identifies the deposit: `<network>:<tx hash>` for a native deposit,
  `<network>:<tx hash>:<log index>` for a token deposit. `(id, event)` is unique.
- `REQ-WH-3` Fields of both events: `id`, `chain`, `chain_id`, `event`, `account_id` (the deposit address),
  `registration_id`, `amount` (minor units, decimal string), `token_type` (`native` or `erc20`), and for tokens
  `token_symbol`, `token_address`, `token_decimals`. The deposit's transaction hash is `tx_hash` in `deposit_detected`
  and `original_tx_hash` in `deposit_swept`.
- `REQ-WH-4` A token `deposit_swept` adds `sweep_tx_hash`; a native one carries no sweep fields. A
  token `deposit_swept` also adds `sweeps`, a list with one entry per sweep that settled the deposit, each the sweep
  coordinate of `REQ-SW-10`: `{sweep_tx_hash, sweep_block_number, sweep_block_hash, sweep_log_index, sweep_amount}`.
  It usually has one entry; it has several when the amount was split by a target's maximum (`REQ-SW-7`). The
  entries are ordered by part. The top-level `sweep_tx_hash` is the first part's sweep transaction on the deposit's
  network: the first entry's for a `transfer`; **(not built, amendment 0008)** the first part's burn for `cctp_burn`
  (`REQ-WH-12`). Several deposits settled by one sweep produce one `deposit_swept`
  each, all naming the same entry. `deposit_swept` is sent only once every sweep of the deposit has succeeded.
  (Revised by amendment 0003.)
- `REQ-WH-9` Every deposit records the sender of its inbound transfer, and both events carry it as
  `from_address`. A consumer uses it as the refund address.
- `REQ-WH-5` Once a webhook is in the outbox, delivery is at least once: a worker delivers it, retrying non-2xx answers
  up to a configured number of attempts, and leases each attempt so that two workers never post the same delivery at
  once. In 0.1.0 the outbox write happens after the state change it reports, and a failed write is only logged.
- `REQ-WH-7` The outbox write MUST be in the same transaction as the state change it reports, so a
  stored deposit or sweep always has its webhook.
- `REQ-WH-8` **(not built)** The bearer token sent to webhook URLs and the operator token MUST be different secrets.
  The operator token MUST never be sent to a webhook URL.
- `REQ-WH-6` A receiver MUST treat `(id, event)` as its idempotency key.
- `REQ-WH-10` A sweep target MAY declare a notify URL and the name of the environment variable holding its bearer
  token. Each token `deposit_swept` for a deposit settled into that target is then also delivered there: the same
  body, an outbox row of its own (`(id, "deposit_swept:notify")`) written in the same transaction, the target's own
  token, and the delivery rules of `REQ-WH-5`, retried for at least two hours, then `notify_failed`. evmhot refuses to
  start when a notify URL's token is empty. The swap
  service receives bezent's Safe sweeps this way (R12). (Amendment 0005.)
- `REQ-CF-1` A `[[chains]]` table with a key evmhot does not know MUST be refused at load, so configuration never
  claims behaviour the binary lacks. `rpc_urls` and `faucet_low_balance` are known keys. (Amendment 0005, 0006, 0007.)
- `REQ-CF-2` A chain MAY list several RPC endpoints (`rpc_url` first, then `rpc_urls`); with several, 
  `block_offset_from_head` MUST be at least 10. A call moves to the next endpoint on a 429, 401-404, 413, 5xx, transport
  error (30 s request timeout), or a refusal of the method, range or size; a revert never fails over. An endpoint that
  answered 429 cools down for 10 s; one that answered 401-404 or 413 for 60 s. A chain with one URL has no failover but
  is subject to `REQ-DT-11` all the same. (Amendment 0006.)
- `REQ-WH-11` The token `deposit_swept` body is published as a versioned contract in `contracts/deposit_swept/`
  (JSON Schema, fixtures, changelog); a test checks the payload evmhot builds against it. Consumers ignore unknown
  fields; an additive change bumps the minor version. (Amendment 0005.) **(not built, amendment 0008)** A new field
  that changes how existing ones are read is enabled only once its consumers confirm it (`REQ-WH-12`).
- `REQ-WH-12` **(not built)** A `cctp_burn` deposit's `deposit_swept` is sent once every part's mint has passed. Its
  top-level fields describe the deposit on its own network, and its top-level `sweep_tx_hash` is the first burn. Each
  `sweeps[]` entry adds `sweep_network` (the destination) and `burn_tx_hash` (that part's burn); its `sweep_*`
  coordinate, `sweep_tx_hash` and `sweep_target` are the mint's. An entry without `sweep_network` is on the deposit's
  own network. Contract 1.1.0. A `cctp_burn` target MUST NOT be enabled until every consumer of its clients'
  `deposit_swept` (`REQ-SW-19`; their registration webhooks and the target's notify URL) has confirmed it reads
  1.1.0. (Amendment 0008.)

## 9. HTTP surface

All bodies are JSON unless stated. The surface is meant for an internal network; §12 states what it does not protect.

### 9.1 The `/evm` surface clients use

In the deployment that ran, a host service served these routes. Each acts on one **default network** (configured)
where it takes no network.

| Method and path | Request | Response |
|---|---|---|
| `GET /evm/health` | | `200 {status, enabled}`; `status` is `"error"` when the check fails, `"disabled"` when the wallet is off |
| `POST /evm/register` | `{id, webhook_url}` | `{address}`; the `funding_tx` key is never present (`REQ-RG-4`). `503` while the store's write queue is full |
| `POST /evm/verify_transfer` | `{chain, tx_hash, to_address, amount, token_type, token_address?, token_symbol?}` | `{status: "success", actual_to, actual_amount, token_type, token_symbol?, block_number?}` or `{status: "error", message, token_type?, block_number?}`. Reads the chain; writes nothing |
| `GET /evm/block_number` | | `{block_number}`: the default network's cursor |
| `POST /evm/block_number` | `{block_number}` | `{block_number}`: moves the default network's cursor. An operator action |

When the wallet is disabled, `register` and both `block_number` routes answer `503`, and `verify_transfer` answers
`500`. Errors are the status code with a JSON body `{error, status}`: the message and the numeric status.

### 9.2 Operator routes

| Method and path | Request | Response |
|---|---|---|
| `POST /admin/retry_sweeps` | `{chain, tx_hash, log_index?}` | `{retried, token_type}` |
| `POST /admin/retry_webhooks` | `{id, event}` | `{retried, status}` |

Both require the bearer token. The host service did not serve them in production. `retry_sweeps` moves a token or native
deposit in `needs_review` or `failed` back to `detected` in one transaction, refuses while an intent covering it is
pending, first re-reads the receipts of every stored hash of the deposit's intents (a matching success
settles it, a non-matching success refuses the retry), and closes its `review` intents (`closed_by_operator`),
never re-opening one. What is owed is then computed
from recorded sweeps alone; retrying a review whose sweep in fact moved the funds is the operator's word that nothing
more is owed (amendment 0004).

- `REQ-API-1` A standalone evmhot MUST serve §9.1 under `/evm` with exactly those shapes, so that existing clients only
  change their base URL. It MAY also serve per-network forms of `block_number` that take a `chain`.
- `REQ-API-2` **(not built)** `POST /evm/block_number` MUST require the operator's bearer token.

## 10. Store

- `REQ-ST-1` The store holds: registrations (id, derivation index, address, webhook URL); native and token deposits
  with their status; sweep bookkeeping; the outbox; and a key-value state with each network's cursor and the
  derivation counter.
- `REQ-ST-2` The store is the only record that maps an address to its derivation index. Losing it strands every
  deposit address whose index is not otherwise known. It MUST be backed up, and any move to a new database MUST carry
  every row.

## 11. Observability (amendment 0001)

- `REQ-OB-1` **(not built)** Per network, evmhot MUST expose how far the cursor trails the detection height, and MUST
  raise an alert when that lag exceeds a configured bound.
- `REQ-OB-2` **(not built)** A provider answer that says the caller is rate-limited or over quota MUST be retried with
  growing back-off, and MUST raise an alert when it persists.
- `REQ-OB-4` An alert is a log line starting `EVMHOT_ALERT kind=<kind> network=<network>`, with the deposit's identity
  and transaction where there are some. Kinds: `sweep_review`, `balance_shortfall`, `no_sender`, `pair_blocked`
  (review or failed blockers only), `dust`, `settled_without_sweep`, `native_sweep_failed`, `review_self_resolved`,
  `sweep_stuck`, `notify_failed`, `faucet_low` (amendment 0007), `contract_sender`, `sender_unchecked`, `cctp_unminted`, `cctp_config` and `relay_failing` (amendment 0008, not built), and with `REQ-OB-1`/`REQ-OB-2` `lag` and `provider`. `dust` and `pair_blocked` are raised once per deposit
  and blocking state. (Amendment 0004.)
- `REQ-FC-4` Per network, evmhot reads the faucet's balance every 5 minutes and raises `faucet_low` when it is below
  the chain's `faucet_low_balance` (wei; default 20 × `existential_deposit`; zero is refused at load): on the first
  low reading, then hourly while it stays low. A reading back at or above the floor is logged and closes the spell. A
  failed read is a WARN, not an alert. The check never changes what the faucet funds. (Amendment 0007.)
- `REQ-OB-3` Health reports the store's writer and lists the networks. **(not built)** It also reports whether each
  network's provider answers.

## 12. Invariants

- `INV-1` A registration's address never changes.
- `INV-2` A deposit is recorded at most once per identity (`REQ-DT-2`, `REQ-DT-3`).
- `INV-3` A derivation index belongs to at most one registration.
- `INV-4` No key or mnemonic leaves the process (`REQ-KY-3`).
- `INV-5` A deposit marked swept with a sweep transaction has a successful receipt for that
  transaction (`REQ-SW-5`). **(not built)** A deposit swept by `cctp_burn` has, for each part, an accepted burn receipt
  and a passed mint receipt holding its `Transfer` (`REQ-SW-16`, `REQ-SW-18`).
- `INV-6` No sweep sends a token to a target that does not admit it (`REQ-SW-8`).
- `INV-7` No deposit is marked swept (by a receipt or by `REQ-SW-3`) or released by an operator while a sweep intent
  covering it is pending, and a deposit is settled only by its recorded sweeps once nothing is left owed
  (`REQ-SW-13`). **(not built)** Except that a passed mint relayed by someone else settles its `cctp_burn` part while
  the relayer's intent for it is open (`REQ-SW-18`).
- `INV-8` Every transaction mined from a deposit address is a recorded sweep, a recorded approval or accepted burn
  (not built, `REQ-SW-16`), or belongs to an open intent (`REQ-SW-13`).
- `INV-9` **(not built)** Every transaction mined from the relayer belongs to a recorded relay intent (`REQ-SW-18`).

What this version does not protect, stated plainly:

- `/register` and `/verify_transfer` are unauthenticated and rely on the network they are served on. Any caller there can
  register an id and choose its webhook URL, so evmhot will post to any address that caller names.
- Reorganisations are not handled: the detection height is the only protection (`REQ-DT-7`, `REQ-DT-8`).
- Moving a cursor backwards replays detection; `INV-2` stops duplicate deposits, and delivered webhooks are not sent
  again.

## 13. Known differences from 0.1.0 (what ran)

The service as it ran differs from this version where a requirement is marked **(not built)**:

- A deposit was marked swept once its sweep was mined, without checking the receipt's status (`REQ-SW-5`).
- `deposit_swept` carried no sweep block, block hash, log index or amount (`REQ-SW-10`, `REQ-WH-4`).
- Detection used the newest block minus an offset on every network (`REQ-DT-8`).
- There was no lag alert, no back-off on rate limits and no alert on quota exhaustion (`REQ-OB-1`, `REQ-OB-2`).
- Sweep targets were addresses only, with no bounds or token admission checks (`REQ-SW-6` to `REQ-SW-9`).
- `POST /evm/block_number` was unauthenticated (`REQ-API-2`).
- Token transfers with a symbol longer than five characters, or with no readable symbol, were dropped (`REQ-DT-10`,
  withdrawn).
- The outbox write was not atomic with the state change (`REQ-WH-7`).
- The webhook bearer token was also the operator token (`REQ-WH-8`).
- Health did not check providers (`REQ-OB-3`), and the derivation counter had no ceiling check (`REQ-RG-5`).

## 14. Open questions

None open in this version. Rulings are recorded in `amendments/`.
