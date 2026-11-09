# evmhot — System Specification

**Version:** 0.2.0 · **Status:** draft for review · **Owner:** @pandres95

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
- Attributing a sweep to a party beyond the registration id. evmhot keeps no sender lists and pushes no addresses
  to other systems.
- Prices, conversions, fees charged to a party, refunds.
- Custody of the sweep targets.

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
| **detection height** | The newest block the monitor may process, per §7.1 |
| **sweep coordinate** | The block number, block hash and block-level log index of the token `Transfer` a sweep emitted into its target |
| **outbox** | The stored queue of webhook deliveries |

## 3. Parties

- **Client.** A service that registers parties and receives webhooks. It is trusted to send unique registration
  ids; it is not trusted with keys.
- **Verifier.** A service that asks whether a transaction moved funds to an address.
- **Operator.** A person who sets configuration, retries a stuck sweep or webhook, and moves a cursor.
- **Network provider.** The RPC endpoint per network. Trusted for reads only up to the finality rule in §7.1.

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
  `m/44'/60'/0'/0/<derivation index>`.
- `REQ-KY-2` The faucet MUST use a separate mnemonic.
- `REQ-KY-3` Neither mnemonic, nor any key derived from them, MAY be logged, returned by the API, written to the
  store, or included in a webhook.
- `REQ-KY-4` A change of deposit mnemonic strands every existing address. The deposit mnemonic MUST NOT change while
  any registration exists.

### 5.2 Registration

- `REQ-RG-1` `register(id, webhook_url)` returns the deposit address for `id`. A repeated call with the same `id`
  returns the same address and changes nothing.
- `REQ-RG-2` A new registration takes the next derivation index from a stored counter, allocated atomically with
  the registration, so two registrations never share an index.
- `REQ-RG-3` The counter MUST start above every derivation index already in the store. Indices are not contiguous:
  older registrations used hashed indices up to 2^31.
- `REQ-RG-4` Registration funds nothing. Gas is funded at sweep time (§6.3).
- `REQ-RG-5` **(not built)** Derivation indices are non-hardened, so below 2^31. When the counter reaches 2^31,
  registration MUST be refused with an error; it MUST NOT wrap or overflow.

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

### 6.2 Sweeps

- `REQ-SW-1` In 0.1.0 a sweep moves the deposit address's **whole** balance of the token, not the deposit's amount.
  Several deposits of one token to one address can therefore be settled by one sweep; each of them is then marked swept
  by that sweep. `REQ-SW-12` overrides this for targets that need a refund address.
- `REQ-SW-2` A sweep goes to the token's sweep target on that network. A token with no target of its own uses the
  network's default target. The native coin always uses the default target.
- `REQ-SW-3` A token deposit whose address holds a zero balance of the token is retried for 10 passes and then marked
  swept with no sweep transaction. Native deposits have no such rule.
- `REQ-SW-4` A sweep that fails with a funding error waits for the faucet and is retried. A token sweep that fails
  otherwise counts the failure; after 5 consecutive failures, or at once on a decoding error, the deposit is recorded as
  failed and can be retried by an operator. A native sweep that fails is retried on every pass.
- `REQ-SW-11` A native sweep sends the balance minus the cost of 21,000 gas at the current maximum fee plus 10%. A
  balance at or below that cost is not swept.
- `REQ-SW-5` **(not built)** A deposit MUST be marked swept only after its sweep's receipt shows success. A reverted
  sweep is recorded as failed.

### 6.3 Gas

- `REQ-FC-1` Before a sweep, the sweeper checks the deposit address's native balance against the gas the sweep needs,
  and asks the faucet to top it up if short.
- `REQ-FC-2` A faucet with too little balance makes sweeps wait; it never marks a deposit swept.

### 6.4 Sweep targets (amendment 0001)

- `REQ-SW-6` A sweep target has a kind:
  - `transfer`: an address. The sweep is a token transfer to it. This is the only kind built today.
  - `directed_deposit` **(not built)**: a contract and a 32-byte recipient. The sweep calls the contract's deposit
    function with the amount and the recipient. The recipient is configuration, one per network and token, and is
    the same for every deposit address.
- `REQ-SW-7` **(not built)** A target MAY declare a minimum and a maximum per sweep, in the token's minor units:
  - A balance below the minimum MUST NOT be swept to that target. It stays in the deposit address until later
    deposits bring it to the minimum.
  - A balance above the maximum MUST be swept in several sweeps, each at most the maximum.
- `REQ-SW-8` **(not built)** A target MAY declare that it admits only one token. Configuration that routes any other
  token to it MUST be refused at load time, and the process MUST NOT start.
- `REQ-SW-9` **(not built)** Each sweep into a target emits exactly one token `Transfer` into it.

- `REQ-SW-12` **(not built)** A target MAY declare that each sweep needs one refund address. For such a target, a
  sweep MUST NOT combine deposits from different senders: each sweep covers one inbound transfer, or several from the
  same sender. Deposits from one sender below the target's minimum MAY accumulate until together they reach it.

### 6.5 The sweep coordinate (amendment 0001)

- `REQ-SW-10` **(not built)** For each sweep evmhot records, and reports in `deposit_swept`, the sweep's
  transaction hash, block number, block hash, the block-level log index of the `Transfer` into the target, and the
  amount swept. The log index is the position of that log in its block, taken from the receipt, not its position
  within the transaction.

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
- `REQ-WH-4` A token `deposit_swept` adds `sweep_tx_hash`; a native one carries no sweep fields.
- `REQ-WH-9` **(not built)** Every deposit records the sender of its inbound transfer, and both events carry it as
  `from_address`. A consumer uses it as the refund address. **(not built)** It also adds the sweep coordinate of `REQ-SW-10`:
  `sweep_block_number`, `sweep_block_hash`, `sweep_log_index` and `sweep_amount`. Several deposits settled by one
  sweep produce one `deposit_swept` each, all naming the same sweep coordinate.
- `REQ-WH-5` Once a webhook is in the outbox, delivery is at least once: a worker delivers it, retrying non-2xx answers
  up to a configured number of attempts, and leases each attempt so that two workers never post the same delivery at
  once. In 0.1.0 the outbox write happens after the state change it reports, and a failed write is only logged.
- `REQ-WH-7` **(not built)** The outbox write MUST be in the same transaction as the state change it reports, so a
  stored deposit or sweep always has its webhook.
- `REQ-WH-8` **(not built)** The bearer token sent to webhook URLs and the operator token MUST be different secrets.
  The operator token MUST never be sent to a webhook URL.
- `REQ-WH-6` A receiver MUST treat `(id, event)` as its idempotency key.

## 9. HTTP surface

All bodies are JSON unless stated. The surface is meant for an internal network; §12 states what it does not protect.

### 9.1 The `/evm` surface clients use

In the deployment that ran, a host service served these routes. Each acts on one **default network** (configured)
where it takes no network.

| Method and path | Request | Response |
|---|---|---|
| `GET /evm/health` | | `200 {status, enabled}`; `status` is `"error"` when the check fails, `"disabled"` when the wallet is off |
| `POST /evm/register` | `{id, webhook_url}` | `{address, funding_tx}`; `funding_tx` is always null (`REQ-RG-4`). `503` while the store's write queue is full |
| `POST /evm/verify_transfer` | `{chain, tx_hash, to_address, amount, token_type, token_address?, token_symbol?}` | `{status: "success", actual_to, actual_amount, token_type, token_symbol?, block_number?}` or `{status: "error", message, token_type?, block_number?}`. Reads the chain; writes nothing |
| `GET /evm/block_number` | | `{block_number}`: the default network's cursor |
| `POST /evm/block_number` | `{block_number}` | `{block_number}`: moves the default network's cursor. An operator action |

When the wallet is disabled, every route but health answers `503`. Errors are a status code with a plain-text body.

### 9.2 Operator routes

| Method and path | Request | Response |
|---|---|---|
| `POST /admin/retry_sweeps` | `{chain, tx_hash, log_index?}` | `{retried, token_type}` |
| `POST /admin/retry_webhooks` | `{id, event}` | `{retried, status}` |

Both require the bearer token. The host service did not serve them in production.

- `REQ-API-1` A standalone evmhot MUST serve §9.1 under `/evm` with exactly those shapes, so that existing clients only
  change their base URL. It MAY also serve per-network forms of `block_number` that take a `chain`.
- `REQ-API-2` **(not built)** `POST /block_number` MUST require the operator's bearer token.

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
- `REQ-OB-3` Health reports the store's writer and lists the networks. **(not built)** It also reports whether each
  network's provider answers.

## 12. Invariants

- `INV-1` A registration's address never changes.
- `INV-2` A deposit is recorded at most once per identity (`REQ-DT-2`, `REQ-DT-3`).
- `INV-3` A derivation index belongs to at most one registration.
- `INV-4` No key or mnemonic leaves the process (`REQ-KY-3`).
- `INV-5` **(not built)** A deposit marked swept with a sweep transaction has a successful receipt for that
  transaction (`REQ-SW-5`).
- `INV-6` **(not built)** No sweep sends a token to a target that does not admit it (`REQ-SW-8`).

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
- `POST /block_number` was unauthenticated (`REQ-API-2`).
- Token transfers with a symbol longer than five characters, or with no readable symbol, were dropped (`REQ-DT-10`,
  withdrawn).
- The outbox write was not atomic with the state change (`REQ-WH-7`).
- The webhook bearer token was also the operator token (`REQ-WH-8`).
- Health did not check providers (`REQ-OB-3`), and the derivation counter had no ceiling check (`REQ-RG-5`).

## 14. Open questions

None open in this version. Rulings are recorded in `amendments/`.
