# EVM Hot Wallet Service

A Rust-based hot wallet service for EVM-compatible blockchains that monitors deposits and automatically sweeps funds to a treasury address. Built with performance, safety, and reliability in mind.

## Features

- 🔍 **Real-time Monitoring**: Dual-mode blockchain monitoring with WebSocket subscriptions and HTTP polling fallback
- 💸 **Automatic Sweeping**: Automatically sweeps detected deposits to a configured treasury address
- 🚰 **Faucet Integration**: Built-in faucet for funding new addresses with existential deposits
- 🔐 **HD Wallet Support**: BIP-39 mnemonic-based hierarchical deterministic wallet for generating unique addresses
- 📡 **REST API**: Simple API for registering users and generating deposit addresses
- 🪝 **Per-Account Webhooks**: Custom webhook URLs per user for deposit detection and sweep notifications
- 🗄️ **Embedded Database**: Uses `redb` for efficient, embedded storage
- 🪙 **ERC-20 Support**: Monitors and sweeps both native ETH and ERC-20 token deposits
- 🧪 **Well-Tested**: Comprehensive unit and E2E tests with mocked providers
- 🚀 **CI/CD Ready**: GitHub Actions workflow for formatting, linting, and testing

## Architecture

The service consists of four main components:

### 1. Monitor
Monitors the blockchain for incoming transactions to registered addresses:
- **WebSocket Mode**: Real-time block subscriptions for instant deposit detection
- **HTTP Polling Mode**: Fallback polling mechanism with configurable intervals
- **Native ETH & ERC-20**: Detects both native token and ERC-20 token transfers
- **Smart Filtering**: Automatically ignores deposits from the faucet address to prevent sweeping existential deposits
- Tracks last processed block to handle restarts gracefully
- Records detected deposits in the database with token metadata

### 2. Sweeper
Processes detected deposits and transfers funds to the treasury:
- Retrieves pending deposits from the database
- Derives private keys for each deposit address
- **Native ETH**: Calculates gas costs and transfers maximum available balance
- **ERC-20 Tokens**: Sweeps ERC-20 tokens (requires native balance for gas)
- Sends webhook notifications on successful sweeps
- Marks deposits as swept in the database

### 3. Faucet
Automatically funds newly registered addresses with an existential deposit:
- Uses a separate mnemonic for security isolation
- Sends configurable amount to new addresses upon registration
- Ensures addresses have sufficient balance for future transactions
- Faucet deposits are automatically excluded from sweeping

### 4. API Server
HTTP API for user management and address generation:
- `POST /register` - Register a new user with a webhook URL and receive a unique deposit address
- Deterministic address derivation using hash-based indexing
- Automatic funding via faucet upon registration
- Per-account webhook configuration for custom notification endpoints
- Thread-safe database access

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

The service is configured via environment variables. Create a `.env` file or set these variables:

### Required Variables

| Variable | Description | Example |
|----------|-------------|---------|
| `MNEMONIC` | BIP-39 mnemonic phrase for HD wallet (used to derive user deposit addresses) | `test test test test test test test test test test test junk` |
| `FAUCET_MNEMONIC` | BIP-39 mnemonic phrase for faucet wallet (used to fund new addresses) | `another twelve word phrase for faucet` |
| `FAUCET_ADDRESS` | Ethereum address of the faucet (derived from `FAUCET_MNEMONIC` at index 0) | `0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266` |
| `TREASURY_ADDRESS` | Ethereum address where funds will be swept | `0x742d35Cc6634C0532925a3b844Bc9e7595f0bEb` |
| `RPC_URL` or `WS_URL` | Blockchain node endpoint (use WS for real-time, RPC for polling) | `https://eth-mainnet.g.alchemy.com/v2/...` or `wss://eth-mainnet.g.alchemy.com/v2/...` |

### Optional Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `DATABASE_URL` | Path to the database file | `sqlite:wallet.db` |
| `PORT` | API server port | `3000` |
| `POLL_INTERVAL` | Block polling interval in seconds (HTTP mode only) | `10` |
| `BLOCK_OFFSET_FROM_HEAD` | Number of blocks to stay behind chain head for confirmation safety | `20` |
| `EXISTENTIAL_DEPOSIT` | Amount in wei to fund new addresses with | `10000000000000000` (0.01 ETH) |

### Example `.env` File

```env
# Database
DATABASE_URL=sqlite:wallet.db

# Blockchain Connection (choose one)
RPC_URL=https://polygon-mainnet.g.alchemy.com/v2/YOUR_API_KEY
# For WebSocket (comment out RPC_URL if using WS):
# WS_URL=wss://polygon-mainnet.g.alchemy.com/v2/YOUR_API_KEY

# Hot Wallet Configuration
# This mnemonic is used to derive deposit addresses for users
MNEMONIC=your twelve word mnemonic phrase goes here for hot wallet

# Faucet Configuration
# This mnemonic is for the faucet that funds new addresses with existential deposit
FAUCET_MNEMONIC=another twelve word mnemonic phrase for faucet wallet funding

# Faucet Address (derived from FAUCET_MNEMONIC at index 0)
# This address is used to identify and skip faucet deposits from being swept
# To get this address: derive it from your FAUCET_MNEMONIC using BIP39/BIP44 at path m/44'/60'/0'/0/0
FAUCET_ADDRESS=0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266

# Existential Deposit (in wei)
# Default: 10000000000000000 (0.01 ETH on Ethereum)
# Adjust based on network: lower for testnets, consider gas costs
EXISTENTIAL_DEPOSIT=10000000000000000

# Treasury address where funds are swept to
TREASURY_ADDRESS=0x742d35Cc6634C0532925a3b844Bc9e7595f0bEb

# API Server Port
PORT=3000

# Polling interval in seconds (for monitoring new blocks)
POLL_INTERVAL=10

# Block Offset from Head (number of blocks behind current head for confirmation safety)
BLOCK_OFFSET_FROM_HEAD=20
```

## Usage

### Running the Service

```bash
# With .env file
cargo run --release

# Or with environment variables
MNEMONIC="..." \
TREASURY_ADDRESS="0x..." \
WEBHOOK_URL="https://..." \
RPC_URL="https://..." \
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
  "address": "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
  "funding_tx": "0xabc123..." // Optional: transaction hash of faucet funding
}
```

**Note**: Upon registration, the address is automatically funded with the configured existential deposit from the faucet. This ensures the address has enough balance for gas fees when sweeping deposits.

**Important**: Each user registers with their own `webhook_url`. This allows per-user notification endpoints for deposit detection and sweep events.

### Webhook Notifications

The service sends webhook notifications to the per-account `webhook_url` for two types of events:

#### 1. Deposit Detection
When a deposit is first detected on the blockchain, a POST request is sent to the account's webhook URL:

**Native ETH Deposit Detected:**
```json
{
  "event": "deposit_detected",
  "account_id": "user_123",
  "tx_hash": "0xabc...",
  "amount": "1000000000000000000",
  "token_type": "native"
}
```

**ERC-20 Token Deposit Detected:**
```json
{
  "event": "deposit_detected",
  "account_id": "user_123",
  "tx_hash": "0xdef123",
  "amount": "1000000000000000000",
  "token_type": "erc20",
  "token_symbol": "USDC",
  "token_address": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
}
```

#### 2. Deposit Swept
When a deposit is successfully swept to the treasury, a POST request is sent to the account's webhook URL:

**Native ETH Deposit Swept:**
```json
{
  "event": "deposit_swept",
  "account_id": "user_123",
  "original_tx_hash": "0xabc...",
  "amount": "1000000000000000000",
  "token_type": "native"
}
```

**ERC-20 Token Deposit Swept:**
```json
{
  "event": "deposit_swept",
  "account_id": "user_123",
  "original_tx_hash": "0xdef123:0",
  "amount": "1000000000000000000",
  "token_type": "erc20",
  "token_symbol": "USDC",
  "token_address": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
}
```

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
- Ensure the faucet address has enough native currency to fund new addresses
- Each registration requires at least `EXISTENTIAL_DEPOSIT` amount

**"ERC-20 sweep fails with insufficient gas"**
- Addresses need native balance (ETH/MATIC/etc.) to pay for ERC-20 transfer gas
- Consider increasing `EXISTENTIAL_DEPOSIT` if you expect ERC-20 deposits

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
│   ├── db.rs            # Database layer (redb)
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
- **redb**: Embedded key-value database
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
8. **Faucet funding** - Ensure the faucet address is properly funded to support new user registrations
9. **Correct FAUCET_ADDRESS** - Double-check that `FAUCET_ADDRESS` matches the address derived from `FAUCET_MNEMONIC` at index 0

## How It Works

### Registration Flow
1. User calls `POST /register` with their account ID and webhook URL
2. System derives a deterministic address using hash-based indexing
3. Faucet automatically sends existential deposit to the new address
4. Address and webhook URL are registered in the database
5. Address is ready to receive deposits with custom webhook notifications

### Deposit Detection & Sweeping Flow
1. **Monitor** watches the blockchain for transactions to registered addresses
2. When a deposit is detected, **Monitor checks if it's from the faucet**:
   - If yes: Skip recording (prevents sweeping existential deposits)
   - If no: 
     - Record the deposit in the database
     - Send "deposit_detected" webhook to the account's webhook URL
3. **Sweeper** processes recorded deposits:
   - Derives the private key for the deposit address
   - Calculates gas costs
   - Transfers funds to the treasury address
   - Sends "deposit_swept" webhook to the account's webhook URL
4. Deposit is marked as "swept" in the database

### ERC-20 Token Support
- Monitor detects ERC-20 `Transfer` events to registered addresses
- Fetches and caches token metadata (symbol, decimals, name)
- Sweeper transfers ERC-20 tokens using the native balance for gas
- Separate webhook notifications for ERC-20 sweeps with token details

## Docker Deployment

### Quick Start with Docker Compose

1. **Copy the environment template:**
```bash
cp env.docker.example .env
# Or use: make setup
```

2. **Edit `.env` with your configuration:**
```bash
# Set your RPC endpoint, mnemonics, addresses, etc.
nano .env
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

**Backup the database:**
```bash
docker cp evm-hot-wallet:/app/data/wallet.db ./backup-wallet.db
```

### Production Deployment Notes

1. **Persistent Storage**: Database is stored in a Docker volume (`wallet-data`) to persist across container restarts
2. **Environment Variables**: All configuration is loaded from `.env` file
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

Note: You may need to implement a `/health` endpoint in the API if it doesn't exist.

## Roadmap

- [x] Support for ERC-20 token sweeping
- [x] Faucet integration for funding new addresses
- [x] Smart filtering to prevent sweeping existential deposits
- [x] Docker deployment
- [x] Per-account webhook URLs
- [ ] Configurable gas price strategies
- [ ] Multi-chain support
- [ ] Admin dashboard
- [ ] Prometheus metrics
- [ ] Health check endpoint

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
