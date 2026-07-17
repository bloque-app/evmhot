# EVM Hot Wallet Service

A Rust-based hot wallet service for EVM-compatible blockchains that monitors deposits and automatically sweeps funds to a treasury address. Built with performance, safety, and reliability in mind.

## Features

- 🔍 **Multi-Chain Monitoring**: One process monitors multiple EVM chains (Base, Polygon, others) via HTTP polling
- 💸 **Automatic Sweeping**: Per-chain sweepers transfer detected deposits to each chain's treasury address
- 🚰 **Lazy Faucet**: Funds deposit addresses with gas just-in-time at sweep time (not at registration)
- 🔐 **HD Wallet Support**: BIP-39 mnemonic-based hierarchical deterministic wallet — same addresses on every EVM chain
- 📡 **REST API**: Simple API for registering users and generating deposit addresses
- 🪝 **Per-Account Webhooks**: Custom webhook URLs per user for deposit detection and sweep notifications
- 🗄️ **Embedded Database**: SQLite (rusqlite, WAL mode)
- 🪙 **ERC-20 Support**: Monitors and sweeps both native ETH and ERC-20 token deposits
- 🧪 **Well-Tested**: Comprehensive unit and E2E tests with mocked providers
- 🚀 **CI/CD Ready**: GitHub Actions workflow for formatting, linting, and testing

## Architecture

The service runs one **Monitor** and one **Sweeper** per configured chain, sharing a single database and HD wallet.

### 1. Monitor (per chain)
Polls the chain's RPC endpoint for incoming transactions to registered addresses:
- **HTTP polling only** with configurable interval and block confirmation offset
- **Native ETH & ERC-20**: Detects both native token and ERC-20 token transfers
- **Smart Filtering**: Ignores deposits from that chain's faucet address
- Per-chain last processed block cursor for graceful restarts
- Records detected deposits with chain-prefixed keys and sends webhooks including `chain` / `chain_id`

### 2. Sweeper (per chain)
Processes detected deposits for its chain only:
- Retrieves pending deposits scoped to the chain
- Derives private keys for each deposit address (same mnemonic, all chains)
- **Native ETH**: Calculates gas costs and transfers maximum available balance to the chain treasury
- **ERC-20 Tokens**: Sweeps ERC-20 tokens (lazy-funds native gas if needed)
- Sends chain-aware webhook notifications on successful sweeps

### 3. Faucet (per chain)
Lazy-funds deposit addresses when a sweep needs gas:
- Uses a separate mnemonic for security isolation
- Sends the chain's configured existential deposit only when sweeping
- Faucet deposits are excluded from sweeping on that chain

### 4. API Server
HTTP API for user management and operations:
- `POST /register` — Register a user with a webhook URL; returns a chain-agnostic deposit address
- `POST /verify_transfer` — Verify a transfer on a specific chain (requires `chain` field)
- `GET/POST /block_number` — Read or set the per-chain block cursor (requires `chain`)
- `GET /health` — Health check listing configured chains

## Installation

### Prerequisites

- Rust 1.70+ (install via [rustup](https://rustup.rs/))
- Access to an EVM-compatible blockchain node (HTTP or WebSocket)

### Building from Source

```bash
# Clone the repository
git clone <repository-url>
cd emvhot

# Build the project
cargo build --release

# Run tests
cargo test
```

## Configuration

Secrets stay in environment variables. Per-chain settings (RPC, treasury, tokens, gas) live in a TOML file.

### Environment variables

| Variable | Required | Description | Default |
|----------|----------|-------------|---------|
| `MNEMONIC` | yes | BIP-39 mnemonic for user deposit address derivation (same addresses on all EVM chains) | — |
| `FAUCET_MNEMONIC` | yes | BIP-39 mnemonic for the faucet wallet (lazy-funds addresses for gas at sweep time) | — |
| `CHAINS_CONFIG` | no | Path to chains TOML file | `chains.toml` |
| `DATABASE_URL` | no | SQLite database file path (`sqlite:` prefix optional) | `sqlite:wallet.db` |
| `DB_READ_POOL_SIZE` | no | Max concurrent SQLite read-pool connections, shared by every chain's monitor/sweeper/webhook-retry loop plus inbound registrations. Raise this if you configure more chains or see `"timed out waiting for connection"` under load | `20` |
| `PORT` | no | API server port | `3000` |
| `WEBHOOK_JWT_TOKEN` | no | Optional JWT sent as `Authorization: Bearer` on webhooks and admin endpoints | — |
| `WEBHOOK_MAX_RETRIES` | no | Max delivery attempts before marking a webhook `failed` | `5` |
| `WEBHOOK_RETRY_DELAY_MS` | no | Delay between delivery attempts in the worker batch | `1000` |
| `WEBHOOK_RETRY_POLL_INTERVAL` | no | Worker poll interval (seconds) when no enqueue/admin notify | `30` |
| `WEBHOOK_RETRY_BATCH_SIZE` | no | Max pending deliveries processed per worker batch | `50` |
| `WEBHOOK_LEASE_SECONDS` | no | Claim lease duration to prevent duplicate POSTs | `60` |
| `LEGACY_CHAIN` | no | Chain name for redb→SQLite importer only | `polygon` |

### Chains file (`chains.toml`)

Copy [`chains.toml.example`](chains.toml.example) to `chains.toml`. Each `[[chains]]` block configures one network:

- `name` — short id used in API/webhooks (`base`, `polygon`, …)
- `chain_id` — EVM chain ID (included in webhooks)
- `rpc_url` — HTTP(S) RPC endpoint (polling only)
- `treasury_address`, `faucet_address`, `existential_deposit`
- `allowed_token_addresses` — required per chain (non-empty)
- Optional: `min_deposits`, `min_deposit_default`, `min_deposit_native`, `poll_interval`, `block_offset_from_head`

### Example `.env`

```env
DATABASE_URL=wallet.db
MNEMONIC=your twelve word mnemonic phrase goes here
FAUCET_MNEMONIC=another twelve word mnemonic phrase for faucet
PORT=3000
CHAINS_CONFIG=chains.toml
```

See [WEBHOOK_SPEC.md](./WEBHOOK_SPEC.md) for chain-aware webhook payloads and id format (`{chain}:{tx_hash}`).

## Migration (redb → SQLite)

One-shot offline cutover from the legacy redb file:

```bash
cp evm_wallet.db evm_wallet.db.bak
cargo run --release --bin migrate_redb_to_sqlite -- \
  --from evm_wallet.db \
  --to wallet.db \
  --legacy-chain polygon
```

Verify block cursors and row counts:

```bash
sqlite3 wallet.db "SELECT key, value FROM state WHERE key LIKE 'last_block:%';"
sqlite3 wallet.db "SELECT chain, status, COUNT(*) FROM deposits GROUP BY 1, 2;"
```

Point `DATABASE_URL` at the new SQLite file (bare path or `sqlite:` prefix), then start the service. Keep the redb backup for at least 7 days.

## Usage

### Running the Service

```bash
# With .env + chains.toml
cargo run --release

# Or with environment variables
MNEMONIC="..." \
FAUCET_MNEMONIC="..." \
CHAINS_CONFIG=chains.toml \
cargo run --release
```

### Registering Users

Use the API to register users with their webhook URL and get unique deposit addresses:

```bash
curl -X POST http://localhost:3000/register \
  -H "Content-Type: application/json" \
  -d '{
    "id": "user_123",
    "webhook_url": "https://api.example.com/webhooks/user_123"
  }'
```

Response:
```json
{
  "address": "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
}
```

**Note**: Registration does **not** fund the address. The sweeper lazy-funds gas on each chain when a deposit is swept. The optional `funding_tx` field is omitted (legacy clients may still see `"funding_tx": null`).

**Important**: Each user registers with their own `webhook_url`. Webhooks include `chain`, `chain_id`, and chain-scoped `id` values — see [WEBHOOK_SPEC.md](./WEBHOOK_SPEC.md).

### Verifying Transfers

```bash
curl -X POST http://localhost:3000/verify_transfer \
  -H "Content-Type: application/json" \
  -d '{
    "chain": "polygon",
    "tx_hash": "0xabc...",
    "to_address": "0x742d35Cc6634C0532925a3b844Bc454e4438f44e",
    "amount": "1000000000000000000",
    "token_type": "native"
  }'
```

### Per-Chain Block Cursor

```bash
# Get last processed block for polygon
curl "http://localhost:3000/block_number?chain=polygon"

# Reset cursor (admin)
curl -X POST http://localhost:3000/block_number \
  -H "Content-Type: application/json" \
  -d '{"chain": "polygon", "block_number": 12345678}'
```

### Webhook Notifications

The service sends webhook notifications to the per-account `webhook_url` for deposit events. Each webhook includes `chain`, `chain_id`, and a chain-scoped `id` field for idempotency.

#### Unique Identifier (`id` field)
- **Native ETH deposits**: `{chain}:{tx_hash}` (e.g. `polygon:0xabc...`)
- **ERC20 deposits**: `{chain}:{tx_hash}:{log_index}` (e.g. `base:0xabc...:0`)

See [WEBHOOK_SPEC.md](./WEBHOOK_SPEC.md) for full payload examples.

#### 1. Deposit Detection
When a deposit is first detected on a chain, a POST request is sent to the account's webhook URL:

**Native ETH Deposit Detected:**
```json
{
  "id": "polygon:0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
  "chain": "polygon",
  "chain_id": 137,
  "event": "deposit_detected",
  "account_id": "user_123",
  "tx_hash": "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
  "amount": "1000000000000000000",
  "token_type": "native"
}
```

**ERC-20 Token Deposit Detected:**
```json
{
  "id": "polygon:0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef:0",
  "chain": "polygon",
  "chain_id": 137,
  "event": "deposit_detected",
  "account_id": "user_123",
  "tx_hash": "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
  "amount": "1000000",
  "token_type": "erc20",
  "token_symbol": "USDC",
  "token_address": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
  "token_decimals": 6
}
```

**Converting ERC-20 amounts to human-readable format:**
```javascript
// For USDC with 6 decimals and amount "1000000"
const humanReadable = amount / Math.pow(10, token_decimals);
// Result: 1.0 USDC
```

#### 2. Deposit Swept
When a deposit is successfully swept to the treasury, a POST request is sent to the account's webhook URL:

**Native ETH Deposit Swept:**
```json
{
  "id": "polygon:0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
  "chain": "polygon",
  "chain_id": 137,
  "event": "deposit_swept",
  "account_id": "user_123",
  "original_tx_hash": "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
  "amount": "1000000000000000000",
  "token_type": "native"
}
```

**ERC-20 Token Deposit Swept:**
```json
{
  "id": "polygon:0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef:0",
  "chain": "polygon",
  "chain_id": 137,
  "event": "deposit_swept",
  "account_id": "user_123",
  "original_tx_hash": "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
  "amount": "1000000",
  "token_type": "erc20",
  "token_symbol": "USDC",
  "token_address": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
  "token_decimals": 6
}
```

Registration does **not** emit a `faucet_funding` webhook. Gas is funded silently at sweep time when needed.

#### Webhook Best Practices

1. **Idempotency**: Use the `id` field as an idempotency key to prevent duplicate processing
2. **Deduplication**: Store processed webhook IDs to avoid reprocessing the same event
3. **Validation**: Verify `account_id` belongs to your system
4. **Acknowledgment**: Return HTTP 2xx status code to confirm receipt
5. **Async Processing**: Process webhooks asynchronously to avoid timeouts

**Example Webhook Handler (Node.js/Express):**
```javascript
app.post('/webhook', async (req, res) => {
  const { id, event, account_id, token_type } = req.body;
  
  // Check for duplicate using id as idempotency key
  const exists = await db.findWebhookById(id);
  if (exists) {
    console.log(`Duplicate webhook ignored: ${id}`);
    return res.status(200).send('OK');
  }
  
  // Store and process webhook
  await db.storeWebhook({ id, ...req.body });
  
  switch (event) {
    case 'deposit_detected':
      await handleDepositDetected(req.body);
      break;
    case 'deposit_swept':
      await handleDepositSwept(req.body);
      break;
  }
  
  res.status(200).send('OK');
});
```

For complete webhook specifications, see [WEBHOOK_SPEC.md](./WEBHOOK_SPEC.md).

## Troubleshooting

### How to Derive Your Faucet Address

The `FAUCET_ADDRESS` must match the address derived from your `FAUCET_MNEMONIC` at index 0. Here's how to get it:

**Using a tool like `cast` (from Foundry):**
```bash
cast wallet address --mnemonic "your faucet mnemonic phrase here" --mnemonic-index 0
```

**Using ethers.js:**
```javascript
const { Wallet } = require('ethers');
const mnemonic = "your faucet mnemonic phrase here";
const wallet = Wallet.fromMnemonic(mnemonic, "m/44'/60'/0'/0/0");
console.log(wallet.address);
```

**Using a BIP39 tool:**
- Path: `m/44'/60'/0'/0/0` (Ethereum standard)
- Index: 0

### Common Issues

**"Deposit from faucet is being swept"**
- Verify that `FAUCET_ADDRESS` matches the actual address derived from `FAUCET_MNEMONIC` at index 0
- Check logs to see which address the faucet is using
- Addresses are case-insensitive but should be in checksummed format

**"Faucet has insufficient balance"**
- Ensure the faucet address has enough native currency on each chain to fund sweeps
- Each lazy fund requires at least that chain's `existential_deposit` amount

**"ERC-20 sweep fails with insufficient gas"**
- Addresses need native balance (ETH/MATIC/etc.) to pay for ERC-20 transfer gas
- Consider increasing `EXISTENTIAL_DEPOSIT` if you expect ERC-20 deposits

**"Deposit detected but never swept after faucet was refilled"**
- ERC20 sweeps retry automatically while `status = 'detected'`. Transient faucet/gas errors no longer count toward the permanent-failure limit.
- If a deposit was marked `failed` before this fix (or after 5 non-funding errors), re-queue it with:

```bash
curl -X POST http://localhost:8080/admin/retry_sweeps \
  -H "Authorization: Bearer $WEBHOOK_JWT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"chain":"base","tx_hash":"0x...","log_index":120}'
```

Omit `log_index` for native deposits. The sweeper picks up re-queued rows on the next poll cycle (~10s by default).

**"Webhook delivery failed"**
- Webhooks are persisted in SQLite and retried by a background worker. Non-2xx responses are treated as failures (including 503).
- Re-queue a permanently failed webhook with:

```bash
curl -X POST http://localhost:8080/admin/retry_webhooks \
  -H "Authorization: Bearer $WEBHOOK_JWT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"id":"base:0xabc:120","event":"deposit_swept"}'
```

Inspect failed deliveries:

```sql
SELECT id, event, status, attempt_count, last_http_status, last_error
FROM webhook_deliveries WHERE status = 'failed';
```

The legacy [`scripts/retry_deposit_webhooks.sh`](scripts/retry_deposit_webhooks.sh) script can still be used for manual replays; prefer the admin API for operational retries.

**Manual SQL recovery** (if the API is unavailable):

```sql
UPDATE erc20_deposits SET status = 'detected'
  WHERE chain = 'base' AND tx_hash = '0x...' AND log_index = 120 AND status = 'failed';
DELETE FROM sweep_failures
  WHERE chain = 'base' AND tx_hash = '0x...' AND log_index = 120;
```

## Debugging

### RPC Request/Response Debugging

This project includes comprehensive RPC debugging capabilities to help you troubleshoot blockchain interactions.

#### Quick Start: Enable RPC Debugging

Use the provided helper scripts:

```bash
# Standard debugging (recommended for most cases)
./run-debug.sh

# Maximum verbosity (shows full request/response bodies)
./run-debug-verbose.sh

# Run the debugging example
RUST_LOG=alloy=debug cargo run --example rpc_debug_example
```

#### Manual Debugging with RUST_LOG

Set the `RUST_LOG` environment variable to control logging verbosity:

```bash
# See all RPC calls with detailed timing
RUST_LOG=alloy_transport_http=debug,alloy_rpc_client=debug cargo run

# Maximum verbosity (includes request/response bodies)
RUST_LOG=alloy=trace cargo run

# Debug specific components
RUST_LOG=evm_hot_wallet::monitor=debug,alloy=debug cargo run
```

#### What You'll See

When RPC debugging is enabled, you'll see output like:

```
[DEBUG alloy_transport_http] sending request: method=eth_getBlockNumber
[TRACE alloy_transport_http] request body: {"jsonrpc":"2.0","method":"eth_getBlockNumber","params":[],"id":1}
[TRACE alloy_transport_http] response body: {"jsonrlog_rpc_call":"2.0","id":1,"result":"0x1234567"}
[DEBUG alloy_transport_http] received response: status=200 duration=45ms
```

#### Log Levels

- `error` - Only errors
- `warn` - Warnings and errors
- `info` - General information (default)
- `debug` - Detailed debugging information, RPC method names and timing
- `trace` - Maximum verbosity (includes full request/response JSON bodies)

#### Common Debugging Scenarios

**Debug block monitoring issues:**
```bash
RUST_LOG=evm_hot_wallet::monitor=debug,alloy_provider=debug cargo run
```

**Debug transaction issues:**
```bash
RUST_LOG=evm_hot_wallet::sweeper=debug,alloy_transport_http=trace cargo run
```

**Debug token transfer detection:**
```bash
RUST_LOG=evm_hot_wallet::monitor=debug,alloy_rpc_client=debug cargo run
```

**Save logs to a file:**
```bash
RUST_LOG=alloy=debug cargo run 2>&1 | tee debug.log
```

For more detailed information, see [DEBUG_RPC.md](./DEBUG_RPC.md).

## Development

### Running Tests

```bash
# Run all tests
cargo test

# Run with output
cargo test -- --nocapture

# Run specific test
cargo test test_monitor_db_operations
```

### Code Quality

The project uses standard Rust tooling:

```bash
# Format code
cargo fmt

# Run linter
cargo clippy -- -D warnings

# Check compilation
cargo check
```

### CI/CD

GitHub Actions automatically runs on every push/PR:
- ✅ Format check (`cargo fmt --check`)
- ✅ Compilation check (`cargo check`)
- ✅ Linting (`cargo clippy`)
- ✅ Tests (`cargo test`)

## Project Structure

```
emvhot/
├── src/
│   ├── main.rs          # Entry point, service orchestration
│   ├── api.rs           # REST API server
│   ├── config.rs        # Configuration management
│   ├── db.rs            # Database layer (SQLite)
│   ├── redb_store.rs    # Legacy redb read/migrate (importer only)
│   ├── redb_import.rs   # redb → SQLite import library
│   ├── monitor.rs       # Blockchain monitoring service
│   ├── sweeper.rs       # Fund sweeping service
│   ├── wallet.rs        # HD wallet implementation
│   ├── traits.rs        # Shared service trait
│   ├── tests.rs         # Unit tests
│   └── e2e_tests.rs     # End-to-end tests
├── .github/
│   └── workflows/
│       └── ci.yml       # CI/CD pipeline
├── Cargo.toml           # Dependencies
└── README.md
```

## Dependencies

Key dependencies:
- **alloy**: Ethereum library for transaction handling and providers
- **axum**: Web framework for the REST API
- **rusqlite**: Embedded SQLite database (WAL mode)
- **tokio**: Async runtime
- **tracing**: Logging and diagnostics

See [`Cargo.toml`](./Cargo.toml) for the complete list.

## Security Considerations

⚠️ **Important Security Notes**:

1. **Never commit your `.env` file** - It contains sensitive mnemonic phrases
2. **Separate mnemonics for security** - Use different mnemonics for the hot wallet and faucet
3. **Use environment-specific mnemonics** - Don't use production mnemonics in development
4. **Secure your webhook endpoint** - Validate webhook signatures in production
5. **Monitor gas prices** - The sweeper uses on-chain gas prices which may be high during congestion
6. **Database backups** - Regularly backup your database to prevent data loss
7. **Hot wallet risks** - This is a hot wallet service; funds are only as secure as the server
8. **Faucet funding** - Keep the faucet wallet funded on every configured chain for lazy gas funding at sweep time
9. **Per-chain faucet_address** - In `chains.toml`, each chain's `faucet_address` must match the address derived from `FAUCET_MNEMONIC` at index 0

## How It Works

### Registration Flow
1. User calls `POST /register` with their account ID and webhook URL
2. System derives a deterministic address (same address on Base, Polygon, and all EVM chains)
3. Address and webhook URL are stored in the database — **no on-chain funding yet**
4. Address is ready to receive deposits on any configured chain

### Deposit Detection & Sweeping Flow
1. Each chain's **Monitor** polls its RPC for transactions to registered addresses
2. When a deposit is detected, **Monitor checks if it's from that chain's faucet**:
   - If yes: Skip recording (prevents sweeping existential deposits)
   - If no: Record with chain-prefixed key and send `deposit_detected` webhook
3. That chain's **Sweeper** processes its pending deposits:
   - Lazy-funds gas from the chain faucet if needed
   - Transfers funds to the chain's treasury address
   - Sends `deposit_swept` webhook with chain-scoped `id`
4. Deposit is marked as swept in the database

**Sweep failure behavior:**
- **Native deposits** stay `detected` and retry every poll cycle until the sweep succeeds.
- **ERC20 deposits** stay `detected` on transient faucet/gas errors and retry indefinitely.
- Other ERC20 errors increment a failure counter; after 5 attempts the deposit is marked `failed` and stops retrying until re-queued via `POST /admin/retry_sweeps`.
- On startup (and periodically when the queue is non-empty), the sweeper logs counts of detected/failed deposits per chain.

### ERC-20 Token Support
- Monitor detects ERC-20 `Transfer` events to registered addresses
- Automatically fetches and caches token metadata (symbol, decimals, name)
- Webhooks include `token_decimals` for easy amount conversion
- Sweeper transfers ERC-20 tokens using the native balance for gas
- Unique identification with `tx_hash:log_index` format for multiple transfers in same transaction

## Docker Deployment

### Quick Start with Docker Compose

1. **Copy the environment template:**
```bash
cp env.docker.example .env
# Or use: make setup
```

2. **Edit configuration:**
```bash
# Set mnemonics in .env
nano .env
# Copy and edit per-chain settings
cp chains.toml.example chains.toml
nano chains.toml
```

3. **Build and start the service:**
```bash
docker-compose up -d
# Or use: make up
```

4. **View logs:**
```bash
docker-compose logs -f evm-hot-wallet
# Or use: make logs
```

5. **Check health:**
```bash
curl http://localhost:3000/health
# Or use: make health
```

6. **Stop the service:**
```bash
docker-compose down
# Or use: make down
```

### Using the Makefile

A Makefile is provided for convenience:

```bash
make help          # Show all available commands
make setup         # Create .env from template
make up            # Start the service
make logs          # View logs
make health        # Check service health
make backup        # Backup database
make restart       # Restart service
make down          # Stop service
make rebuild       # Rebuild and restart
make prod-check    # Verify production configuration
```

### Docker Commands

**Build the image manually:**
```bash
docker build -t evm-hot-wallet .
```

**Run the container:**
```bash
docker run -d \
  --name evm-hot-wallet \
  -p 3000:3000 \
  -v wallet-data:/app/data \
  --env-file .env \
  evm-hot-wallet
```

**Check container health:**
```bash
docker ps
docker logs evm-hot-wallet
```

**Backup the database (WAL-safe):**
```bash
docker exec evm-hot-wallet sqlite3 /app/data/wallet.db "PRAGMA wal_checkpoint(TRUNCATE);"
docker cp evm-hot-wallet:/app/data/wallet.db ./backup-wallet.db
# Or use: make backup
```

### Production Deployment Notes

1. **Persistent Storage**: Database is stored in a Docker volume (`wallet-data`) to persist across container restarts
2. **Environment Variables**: Secrets in `.env`; per-chain settings in mounted `chains.toml`
3. **Network**: Service runs on port 3000 by default (configurable)
4. **Security**: 
   - Never commit `.env` file with real secrets
   - Use Docker secrets or environment variable injection for production
   - Consider using a secrets management service (HashiCorp Vault, AWS Secrets Manager, etc.)
5. **Monitoring**: Add health checks and monitoring solutions (Prometheus, Grafana)
6. **Scaling**: For high availability, consider running multiple instances with a shared database

### Health Check

The docker-compose.yml includes a health check. You can also manually check:

```bash
curl http://localhost:3000/health
```

Note: `/health` returns OK and lists configured chain names.

## Roadmap

- [x] Support for ERC-20 token sweeping
- [x] Faucet integration for funding new addresses
- [x] Smart filtering to prevent sweeping existential deposits
- [x] Docker deployment
- [x] Per-account webhook URLs
- [x] Unique webhook identifiers (`id` field)
- [x] Token decimals in ERC-20 webhooks
- [x] Automatic token metadata caching
- [ ] Webhook signature verification (HMAC)
- [ ] Configurable gas price strategies
- [x] Multi-chain support (Base, Polygon, others via `chains.toml`)
- [ ] Admin dashboard
- [ ] Prometheus metrics
- [ ] Health check endpoint (basic `/health` exists; richer per-chain status planned)

## Contributing

Contributions are welcome! Please ensure:
1. All tests pass: `cargo test`
2. Code is formatted: `cargo fmt`
3. No clippy warnings: `cargo clippy -- -D warnings`
4. Add tests for new features

## License

[Your License Here]

## Support

For issues and questions, please open an issue on GitHub.
