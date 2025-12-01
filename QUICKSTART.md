# Quick Start Guide

## 1. Setup (First Time)

```bash
# Clone repository
git clone <repository-url>
cd evmhot

# Create .env file
make setup
# or: cp env.docker.example .env

# Edit configuration
nano .env
```

## 2. Configure `.env`

Minimum required settings:

```bash
# Blockchain RPC
RPC_URL=https://polygon-mainnet.g.alchemy.com/v2/YOUR_API_KEY

# Wallet mnemonics (KEEP SECRET!)
MNEMONIC=your twelve word mnemonic phrase here
FAUCET_MNEMONIC=another twelve word mnemonic here

# Addresses
FAUCET_ADDRESS=0xYourFaucetAddress
TREASURY_ADDRESS=0xYourTreasuryAddress

# Funding amount (in wei)
EXISTENTIAL_DEPOSIT=10000000000000000
```

## 3. Deploy

```bash
# Start service
make up

# Check it's running
make health
# or: curl http://localhost:3000/health
```

## 4. Test API

### Register a User

```bash
curl -X POST http://localhost:3000/register \
  -H "Content-Type: application/json" \
  -d '{
    "id": "user_123",
    "webhook_url": "https://your-api.com/webhooks"
  }'
```

Response:
```json
{
  "address": "0x...",
  "funding_tx": "0x..."
}
```

### Send a Test Deposit

Send ETH/tokens to the returned address. The service will:
1. Detect the deposit → send webhook with `"event": "deposit_detected"`
2. Sweep to treasury → send webhook with `"event": "deposit_swept"`

## 5. Monitor

```bash
# View logs
make logs

# Check status
make status

# Backup database
make backup
```

## Common Commands

| Command | Description |
|---------|-------------|
| `make help` | Show all commands |
| `make up` | Start service |
| `make down` | Stop service |
| `make logs` | View logs |
| `make restart` | Restart service |
| `make health` | Check health |
| `make backup` | Backup database |
| `make rebuild` | Rebuild and restart |

## Troubleshooting

### Service won't start?
```bash
make logs  # Check for errors
```

### Health check failing?
```bash
docker-compose ps  # Check if container is running
curl http://localhost:3000/health  # Test endpoint
```

### Need to reset?
```bash
make clean  # Remove everything
make build  # Rebuild
make up     # Start fresh
```

## Architecture

```
┌─────────────┐
│   Monitor   │ ──> Watches blockchain for deposits
└─────────────┘     Sends "deposit_detected" webhooks
       │
       ├─> Records deposits in DB
       │
┌─────────────┐
│   Sweeper   │ ──> Processes deposits
└─────────────┘     Transfers to treasury
       │            Sends "deposit_swept" webhooks
       │
┌─────────────┐
│   Faucet    │ ──> Funds new addresses
└─────────────┘     With existential deposit
       │
┌─────────────┐
│     API     │ ──> Registers users
└─────────────┘     Returns deposit addresses
```

## Webhook Events

### 1. Deposit Detected
```json
{
  "event": "deposit_detected",
  "account_id": "user_123",
  "tx_hash": "0xabc...",
  "amount": "1000000000000000000",
  "token_type": "native"
}
```

### 2. Deposit Swept
```json
{
  "event": "deposit_swept",
  "account_id": "user_123",
  "original_tx_hash": "0xabc...",
  "amount": "1000000000000000000",
  "token_type": "native"
}
```

## Security Checklist

- [ ] `.env` file never committed to git
- [ ] Unique mnemonics for production (not test mnemonics)
- [ ] Faucet address properly funded
- [ ] Treasury address is secure
- [ ] Webhook endpoint secured with HTTPS
- [ ] Regular database backups configured
- [ ] Logs monitored regularly

## Next Steps

- See [DEPLOYMENT.md](DEPLOYMENT.md) for production setup
- See [README.md](README.md) for full documentation
- Check logs regularly: `make logs`
- Backup database: `make backup`

## Support

Having issues? Check:
1. Logs: `make logs`
2. Health: `make health`
3. Configuration: `cat .env`
4. Documentation: `README.md` and `DEPLOYMENT.md`

