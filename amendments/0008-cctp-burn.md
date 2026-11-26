# Amendment 0008 — sweep Base USDC over CCTP to the Polygon reserve

**Status:** proposed, applied to `SPEC.md` 0.2.8 as **(not built)**, on the default of the owner's decision form row
`d-cctp-evmhot` (26 Nov); the owner can overrule it there. Revised after the PR #25 design review.
**Target:** `SPEC.md` 0.2.7
**Date:** 2026-11-26
**Rulings applied:** the owner's question "why isn't evmhot directly sweeping into the CCTP?" (26 Nov), Swaps' ask of
26 Nov to take the Base → Polygon leg, and the owner's answer to Swaps' `d-evm-one-key` ("one key, faucet funds it").
This replaces the plan of 9 Nov (Swaps holds Base USDC and bridges it; `q-base-forwarder`), which Swaps withdrew.
**Rationale in one line:** Base USDC only becomes reserve once it reaches Polygon, and evmhot already holds the key
that can burn it and the faucet that pays the gas, so one more hop through another party's wallet adds custody and
nothing else.

The CCTP V2 facts this relies on were read from Circle's `evm-cctp-contracts` source and developer documentation, and
checked read-only on Base and Polygon mainnet on 26 Nov: the contracts are wired to each other, the domains are 6
and 7, the per-message burn limit for USDC is 10,000,000 USDC, and the destination minter maps Base USDC to Polygon
USDC. The implementation re-reads them from the deployed contracts, not from this text.

## A1 — the `cctp_burn` target kind

`REQ-SW-6` gains a fourth kind, `cctp_burn`: a sweep that burns the token on its network through Circle's CCTP V2 and
mints it to a recipient on another configured network (the **destination**). Its configuration:

| Key | Meaning |
|---|---|
| `token_messenger` | CCTP V2 `TokenMessengerV2` on this network |
| `message_transmitter` | CCTP V2 `MessageTransmitterV2` on the destination |
| `destination` | The destination network's name; MUST be a configured network |
| `destination_token` | The token the mint pays on the destination |
| `source_domain`, `destination_domain` | CCTP domain ids |
| `mint_recipient` | The address the mint pays on the destination |
| `attestation_url` | Circle's attestation API base URL |
| `min_finality_threshold` | Default 2000 (standard transfer) |
| `max_fee` | In the token's minor units; default 0 |
| `clients` | The registration clients it serves, by webhook URL origin (A1a) |

At load, refused if any fails:

- the destination is a configured network, and the recipient and destination token are non-zero;
- the network has at least two RPC endpoints (`REQ-SW-17`);
- the target is `per_transfer` (`REQ-SW-15`) and declares a `max`;
- if the destination configures the recipient as a target for the destination token, the `cctp_burn` target admits
  only the token, and its bounds lie within that target's (minimum at least, maximum at most).

Before each burn, evmhot reads: each side's `localDomain`; the messenger's local minter and its burn limit for the
token; the destination minter's local token for (source domain, token), which MUST be the destination token; and,
its minimum fee for the amount. A messenger without a fee switch reverts that call; a revert reads as a zero fee. A
domain or token that differs, a `max` above the burn limit, or a minimum fee above `max_fee` stops burns on that
target and raises `cctp_config`, until a read agrees again. A read that fails in transport means no burn this pass,
not `cctp_config`. These reads are cached for a pass.

The mint's `Transfer` from the zero address to the recipient is the transfer into the target for `REQ-SW-9`, and its
coordinate is the sweep coordinate (`REQ-SW-10`), on the destination network (A4).

Only standard transfers with `max_fee` 0 are built. A fee rule for fast transfers is Swaps' to set later.

## A1a — a target scoped to its clients

New `REQ-SW-19`. A `cctp_burn` target MUST list the registration clients it serves, by the origin (scheme, host and
port) of the registration's webhook URL. A deposit whose registration's webhook origin is not listed is not swept to
it: it uses the token's other target on that network. For Base USDC today that is `hold`. `REQ-SW-2` is revised to
say so.

Not every client of a network reads the same thing. On Base, the reserve's route (Swaps) reads `deposit_swept` 1.1.0,
while other clients (Mediums' virtual accounts and per-order addresses) read only its top-level fields and route
their deposits elsewhere. Swapping their USDC into the reserve would be wrong as well as unreadable. The origin is
what the client chose at registration, so it needs no new API. The limit, stated plainly: registration is
unauthenticated (§12), so a caller that registers with a listed origin has its deposits burned into the reserve, and
the listed client is posted about deposits it never made. That is today's exposure for any webhook URL, and the
reserve's own registration (by Swaps, at bezent) credits only deposits Swaps knows.

## A2 — the burn

New `REQ-SW-16`. A `cctp_burn` sweep is, from the deposit address:

1. when the address's on-chain allowance to the messenger is below the part's amount, an `approve` of the messenger
   for the largest amount: a sweep intent that settles no deposit;
2. then `depositForBurn(amount, destination_domain, mint_recipient, token, destination_caller = 0, max_fee,
   min_finality_threshold)`, one per part.

The allowance is read on chain before each burn, never taken from a stored flag. Both transactions are sweep intents
under `REQ-SW-13`: one open intent per address, an explicit nonce, never dropped once signed. The faucet tops up gas
for each, as for a transfer. Before signing a burn, evmhot records the destination's current head on its intent; the
mint is never before it.

A burn's receipt passes only if both of these hold:

- the messenger's `DepositForBurn` with this token, amount, depositor, recipient, destination domain, destination
  messenger, `destination_caller` 0, `max_fee` and `min_finality_threshold`;
- the token's `Transfer` of that amount from the deposit address to the local minter.

Anything else is `sweep_review` (`REQ-SW-14`). Every receipt of a burn or an approval, a revert included, is acted on
only once its block is at or below the network's detection height (§7). Until then the intent stays open, and a
transaction that a reorganisation removes is broadcast again like any unmined sweep. So a revert that a
reorganisation turns into a success can never be followed by a second burn of the same part.

An accepted burn is **in flight** until its mint passes: its amount has left the address but is not yet a recorded
sweep.

- What a deposit is owed (`REQ-SW-15`) is its amount minus its recorded sweeps minus its parts in flight.
- The `REQ-SW-14` reservation counts parts in flight as paid.
- A deposit with nothing left to burn and a part still in flight is **burned**.
- `REQ-SW-3` never applies to a deposit with a part in flight.
- A deposit is swept only when every part's mint has passed.

`destination_caller` is zero, so anyone may relay the mint. The recipient is fixed in the burn, so a relay by someone
else cannot redirect it.

## A3 — the sender check before a burn

New `REQ-SW-17`. The reserve refunds a Base deposit to its sender's address on Polygon, which is only the same owner
for an account controlled by a key. Before a deposit's first burn, evmhot reads the sender's code on two of the
network's endpoints:

- both empty, or both a delegation designator (`0xef0100` and 20 bytes, EIP-7702): the check passes;
- both answer other code: the deposit goes to `needs_review` with alert `contract_sender`, and is held for an
  operator, as for `no_sender`;
- a read fails, or the two disagree: no burn this pass, and the next pass checks again. After an hour of failing or
  disagreeing reads, `sender_unchecked` is raised for that deposit, then again hourly.

The verdict is stored with the deposit and decided once, so a split deposit is never half burned and half held. An
address with no code that sent USDC is controlled by a key, and with 7702 that key controls the same address on the
destination. The limit, stated plainly: a contract created and destroyed in the same transaction leaves an address
with no code, and passes.

## A4 — attestation and the mint

New `REQ-SW-18`. For each part in flight, evmhot asks the attestation API for the burn's message,
`GET /v2/messages/{source_domain}?transactionHash=<burn>`, no sooner than 10 minutes after the burn, then once a
minute. A 404 or an empty answer means pending. A 429 stops every attestation request for 5 minutes, outside the RPC
pool's backoff. Once the answer is `complete`, evmhot stores the message, its nonce, its `expirationBlock` and the
attestation.

**Finding the mint.** A mint for the part passes only if a destination receipt holds a `MessageReceived` from the
transmitter with this nonce and source domain. Its coordinate is the `Transfer(0x0 → recipient)` of the destination
token after the previous `MessageReceived` in that receipt (or its start) and before this one, for the burned amount
less the fee executed (zero while `max_fee` is 0). One transaction may relay several messages; each part takes the
log inside its own call, never another part's. evmhot looks for the mint in three ways:

- the transaction the attestation API reports as the mint, when it reports one, is checked first;
- otherwise, if the transmitter's `usedNonces` marks the nonce used, `MessageReceived` logs filtered on that nonce
  are searched in bounded ranges, starting at the destination head recorded on the burn's intent (A2);
- otherwise the relayer relays it.

A mint is acted on only once its block is at or below the destination's detection height, since its coordinate is
what consumers key on.

**The relay.** The relayer (`REQ-KY-5`) sends `receiveMessage(message, attestation)` as a relay intent for one part.
Relay intents follow `REQ-SW-13`: recorded before signing, an explicit nonce on the relayer, resent, re-signed and
sent to review by the same rules. An intent's part never changes. Its transactions differ only in fees, or in a
re-attested message for the same part.

- **Before signing**, evmhot simulates the call (`eth_call`) and signs nothing that would revert. A simulated or mined
  revert backs that part off on its own (1, 2, 4 … up to 60 minutes). After 5 reverts it raises `relay_failing`, then
  again hourly. The relay loop takes the oldest part that is ready, so one failing message never starves the others.
- **Before a rebroadcast or re-sign**, evmhot reads `usedNonces` and the message's expiry:
  - **Nonce used by someone else:** the part settles from that mint (`INV-7`). The open relay intent is left to be
    mined; it reverts, which costs little gas, and closes as failed.
  - **Message expired, or within 100 destination blocks of `expirationBlock`:** evmhot asks for re-attestation
    (`POST /v2/reattest/{nonce}`), fetches the message again, and re-signs at the same nonce with the new attestation.
    Both fee fields are raised by at least 10% at once, so the node accepts the replacement. A message with
    `expirationBlock` 0 never expires.
- **A mined relay that reverts** closes its intent as failed. A receipt carries no revert reason, so evmhot then reads
  `usedNonces` and the expiry again. The part stays in flight, and a failed relay never fails the deposit.

**Throughput.** One relay loop per destination owns the relayer and keeps at most one open relay intent, so relays go
one at a time across all deposit addresses. That is enough for today's volume; batching is a later change.

**Unminted.** A part in flight with no passed mint 60 minutes after its burn was accepted raises `cctp_unminted`,
naming the burn, the attestation status and Circle's `delayReason` if given, then again hourly. Funds between a burn
and its mint are on neither network; only Circle's message holds them. An operator's recourse is the alert, then a
relay by hand (anyone may relay, since the destination caller is zero). The next pass finds that mint. No operator
route moves a part in flight.

## A5 — keys

New `REQ-KY-5`. The relayer is the faucet mnemonic's address at index 1. It pays only for relays. The faucet tops it
up on the destination through the same path as a deposit address (`REQ-FC-1`), so its gas is never more than one
top-up ahead. No new secret is needed, and `REQ-KY-3` covers it.

Exactly one process may relay with a relayer key, as with the deposit addresses. Environments therefore MUST use
different faucet mnemonics. Test and production do: their faucets' index-0 addresses differ.

## A6 — `deposit_swept` 1.1.0

New `REQ-WH-12`. A `cctp_burn` deposit's `deposit_swept` is sent once every part's mint has passed, never after a
burn alone.

- Its top-level fields describe the deposit on its own network. The top-level `sweep_tx_hash` (`REQ-WH-4`) becomes,
  for every kind, the first part's sweep transaction on `chain`: for a `transfer` the transfer, as before; for
  `cctp_burn` the first part's burn, never its approval. A 1.0.0 reader still finds a transaction on `chain`. That
  transaction moves nothing into the reserve, so `verify_transfer` with the reserve as `to` fails for it.
- Each `sweeps[]` entry adds `sweep_network` (the destination) and `burn_tx_hash` (that part's burn). Its `sweep_*`
  coordinate, `sweep_tx_hash` and `sweep_target` are the mint's, on the destination.
- An entry without `sweep_network` is on the deposit's own network, so every body sent today is unchanged.

The schema change is additive, but a 1.0.0 consumer that ignores `sweep_network` would read a mint's coordinate as one
on `chain`. So a `cctp_burn` target MUST NOT be enabled until every consumer of its clients' `deposit_swept` (each
registration's webhook client and the target's notify URL, `REQ-WH-10`) has confirmed it reads 1.1.0. The runbook
lists them. Consumers key a sweep on `(sweep_network or chain, sweep_block_hash, sweep_log_index)`.

## A7 — store and invariants

The store gains: each deposit's sender verdict; parts in flight with their burn, message, nonce, expiry and
attestation; relay intents. `REQ-SW-13`'s one-open-intent rule also covers the relayer.

- `INV-5` widens: a deposit swept by `cctp_burn` has, for each part, an accepted burn receipt and a passed mint
  receipt holding its `Transfer`.
- `INV-7` gains an exception: a passed mint relayed by someone else settles its part while the relayer's intent for it
  is still open; that intent is re-signed as A4 says, and no deposit waits on it.
- `INV-8` becomes: every transaction mined from a deposit address is a recorded sweep, a recorded approval, an
  accepted burn, or belongs to an open intent.
- New `INV-9`: every transaction mined from the relayer belongs to a recorded relay intent.

## Out of scope

- The reverse, bUSDC paid out to a Base address. That is a payout from custody, outside `§1.3`. It stays with Swaps.
- Fast transfers and fees (A1).
- Any destination other than one recipient per target.
- Batching several relays into one transaction.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `REQ-SW-16`, `REQ-SW-17`, `REQ-SW-18`, `REQ-SW-19`, `REQ-KY-5`, `REQ-WH-12`, `INV-9` |
| Revised | `REQ-SW-2` (a target scoped to its clients), `REQ-SW-3` (not for a deposit with a part in flight), `REQ-SW-6` (kind `cctp_burn`), `REQ-SW-13` (covers the relayer), `REQ-SW-14` and `REQ-SW-15` (parts in flight count as paid), `REQ-FC-1` (the faucet also funds the relayer), `REQ-WH-4` (the top-level `sweep_tx_hash`), `REQ-WH-11` (contract 1.1.0, and a field that changes how others are read is gated), `REQ-OB-4` (kinds `contract_sender`, `sender_unchecked`, `cctp_unminted`, `cctp_config`, `relay_failing`), `INV-5`, `INV-7`, `INV-8` |
| Withdrawn | none |

`REQ-SW-9` and `REQ-SW-10` keep their text; for `cctp_burn` they apply to the mint, on the destination (A1).

## What this costs, stated plainly

- **An unlimited allowance.** Each deposit address gives Circle's `TokenMessengerV2` an unlimited allowance for the
  token.
  - That trusts the messenger's code and its proxy admin, which is a different key from USDC's own admin.
  - The allowance stays live on addresses that are no longer used.
  - Approving for the exact amount instead would cost one more transaction per sweep.
- **Circle's attestation API.** It is a new external dependency. If it is down, parts wait and `cctp_unminted` says
  so. A message expires about 24 hours after attestation and must then be re-attested. evmhot does that (A4), but
  only while Circle's re-attestation service answers.
- **Funds in flight.** Between a burn and its mint, about 15 to 20 minutes at standard finality (Base's attestation
  takes about 15 to 19), plus the detection height's wait before a burn is accepted.
- **A new funded address.** The relayer holds a little of the destination's native coin.
- **Extra transactions.** An address's first burn needs an approve before it.
- **Shared reserve.** The test environment burns on mainnet into the same reserve. Each environment's swap service
  tells its mints apart by which evmhot reports them.
- **Sender check.** It reads two endpoints once per deposit, and holds every deposit from a contract for a person.
- **Enabling is gated** on every consumer confirming 1.1.0 (A6), and only the listed clients' deposits are burned (A1a).
  Every other client's Base USDC stays held.
