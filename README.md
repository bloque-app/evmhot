# EVM Hot Wallet Service

A Rust-based hot wallet service for EVM-compatible blockchains that monitors deposits and automatically sweeps funds to a treasury address. Built with performance, safety, and reliability in mind.

## Features

- 🔍 **Real-time Monitoring**: Dual-mode blockchain monitoring with WebSocket subscriptions and HTTP polling fallback
- 💸 **Automatic Sweeping**: Automatically sweeps detected deposits to a configured treasury address
- 🔐 **HD Wallet Support**: BIP-39 mnemonic-based hierarchical deterministic wallet for generating unique addresses
- 📡 **REST API**: Simple API for registering users and generating deposit addresses
- 🪝 **Webhooks**: Configurable webhook notifications for successful sweeps
- 🗄️ **Embedded Database**: Uses `redb` for efficient, embedded storage
- 🧪 **Well-Tested**: Comprehensive unit and E2E tests with mocked providers
- 🚀 **CI/CD Ready**: GitHub Actions workflow for formatting, linting, and testing

## Architecture

The service consists of three main components:

### 1. Monitor
Monitors the blockchain for incoming transactions to registered addresses:
- **WebSocket Mode**: Real-time block subscriptions for instant deposit detection
- **HTTP Polling Mode**: Fallback polling mechanism with configurable intervals
- Tracks last processed block to handle restarts gracefully
- Records detected deposits in the database

### 2. Sweeper
Processes detected deposits and transfers funds to the treasury:
- Retrieves pending deposits from the database
- Derives private keys for each deposit address
- Calculates gas costs and transfers maximum available balance
- Sends webhook notifications on successful sweeps
- Marks deposits as swept in the database

### 3. API Server
HTTP API for user management and address generation:
- `POST /register` - Register a new user and receive a unique deposit address
- Automatic derivation index incrementing
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
| `MNEMONIC` | BIP-39 mnemonic phrase for HD wallet | `test test test test test test test test test test test junk` |
| `TREASURY_ADDRESS` | Ethereum address where funds will be swept | `0x742d35Cc6634C0532925a3b844Bc9e7595f0bEb` |
| `WEBHOOK_URL` | URL to receive sweep notifications | `https://api.example.com/webhooks/sweeps` |
| `RPC_URL` or `WS_URL` | Blockchain node endpoint (use WS for real-time, RPC for polling) | `https://eth-mainnet.g.alchemy.com/v2/...` or `wss://eth-mainnet.g.alchemy.com/v2/...` |

### Optional Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `DATABASE_URL` | Path to the database file | `sqlite:wallet.db` |
| `PORT` | API server port | `3000` |
| `POLL_INTERVAL` | Block polling interval in seconds (HTTP mode only) | `10` |

### Example `.env` File

```env
# Blockchain Connection (choose one)
RPC_URL=https://polygon-mainnet.g.alchemy.com/v2/YOUR_API_KEY
# WS_URL=wss://polygon-mainnet.g.alchemy.com/v2/YOUR_API_KEY

# Wallet Configuration
MNEMONIC="your twelve or twenty-four word mnemonic phrase here"
TREASURY_ADDRESS=0x742d35Cc6634C0532925a3b844Bc9e7595f0bEb

# Webhooks
WEBHOOK_URL=https://api.example.com/webhooks/deposit-swept

# Optional
PORT=3000
POLL_INTERVAL=10
DATABASE_URL=sqlite:wallet.db
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

Use the API to register users and get unique deposit addresses:

```bash
curl -X POST http://localhost:3000/register \
  -H "Content-Type: application/json" \
  -d '{"id": "user_123"}'
```

Response:
```json
{
  "address": "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
}
```

### Webhook Payload

When a deposit is successfully swept, the service sends a POST request to your webhook URL:

```json
{
  "event": "deposit_swept",
  "account_id": "user_123",
  "original_tx_hash": "0xabc...",
  "amount": "1000000000000000000"
}
```

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
2. **Use environment-specific mnemonics** - Don't use production mnemonics in development
3. **Secure your webhook endpoint** - Validate webhook signatures in production
4. **Monitor gas prices** - The sweeper uses on-chain gas prices which may be high during congestion
5. **Database backups** - Regularly backup your database to prevent data loss
6. **Hot wallet risks** - This is a hot wallet service; funds are only as secure as the server

## Roadmap

- [ ] Support for ERC-20 token sweeping
- [ ] Configurable gas price strategies
- [ ] Multi-chain support
- [ ] Admin dashboard
- [ ] Prometheus metrics
- [ ] Docker deployment

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
