# EVM Hot Wallet - Deployment Guide

This guide covers deploying the EVM Hot Wallet service using Docker.

## Prerequisites

- Docker 20.10+
- Docker Compose 2.0+
- Access to an EVM-compatible blockchain RPC endpoint
- Two BIP-39 mnemonic phrases (hot wallet + faucet)
- Treasury address for receiving swept funds

## Quick Start

### 1. Clone and Setup

```bash
git clone <repository-url>
cd evmhot
```

### 2. Configure Environment

Copy the Docker environment template:

```bash
cp env.docker.example .env
```

Edit `.env` with your configuration:

```bash
nano .env
```

**Required Configuration:**

```bash
# Blockchain RPC (choose one)
RPC_URL=https://polygon-mainnet.g.alchemy.com/v2/YOUR_API_KEY
# or for WebSocket:
# WS_URL=wss://polygon-mainnet.g.alchemy.com/v2/YOUR_API_KEY

# Hot wallet mnemonic (generates user deposit addresses)
MNEMONIC=your twelve word mnemonic phrase goes here

# Faucet wallet (funds new addresses)
FAUCET_MNEMONIC=another twelve word mnemonic for funding
FAUCET_ADDRESS=0xYourFaucetAddress

# Treasury (receives swept funds)
TREASURY_ADDRESS=0xYourTreasuryAddress

# Amount to fund new addresses (in wei)
EXISTENTIAL_DEPOSIT=10000000000000000
```

### 3. Deploy

```bash
# Start the service
docker-compose up -d

# View logs
docker-compose logs -f

# Check status
docker-compose ps
```

### 4. Test the API

```bash
# Health check
curl http://localhost:3000/health

# Register a new user
curl -X POST http://localhost:3000/register \
  -H "Content-Type: application/json" \
  -d '{
    "id": "user_123",
    "webhook_url": "https://your-api.com/webhooks/deposits"
  }'
```

## Production Deployment

### Security Best Practices

1. **Secrets Management**
   - Use Docker secrets or a secrets manager (Vault, AWS Secrets Manager)
   - Never commit `.env` files to version control
   - Rotate mnemonics regularly

2. **Network Security**
   - Use a reverse proxy (nginx, Traefik) with HTTPS
   - Implement rate limiting
   - Use firewall rules to restrict access

3. **Monitoring**
   - Set up log aggregation (ELK stack, Grafana Loki)
   - Monitor disk usage for database growth
   - Alert on service health check failures

### Using Docker Secrets

Create a `docker-compose.prod.yml`:

```yaml
version: '3.8'

services:
  evm-hot-wallet:
    build: .
    secrets:
      - mnemonic
      - faucet_mnemonic
      - rpc_url
    environment:
      - MNEMONIC=/run/secrets/mnemonic
      - FAUCET_MNEMONIC=/run/secrets/faucet_mnemonic
      - RPC_URL=/run/secrets/rpc_url
      # ... other env vars
    # ... rest of config

secrets:
  mnemonic:
    external: true
  faucet_mnemonic:
    external: true
  rpc_url:
    external: true
```

Create secrets:

```bash
echo "your mnemonic phrase" | docker secret create mnemonic -
echo "your faucet mnemonic" | docker secret create faucet_mnemonic -
echo "https://your-rpc-url" | docker secret create rpc_url -
```

### Reverse Proxy with Nginx

Example nginx configuration:

```nginx
upstream evm_hot_wallet {
    server localhost:3000;
}

server {
    listen 443 ssl http2;
    server_name wallet-api.yourdomain.com;

    ssl_certificate /etc/letsencrypt/live/yourdomain.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/yourdomain.com/privkey.pem;

    # Rate limiting
    limit_req_zone $binary_remote_addr zone=api_limit:10m rate=10r/s;
    limit_req zone=api_limit burst=20 nodelay;

    location / {
        proxy_pass http://evm_hot_wallet;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    location /health {
        proxy_pass http://evm_hot_wallet;
        access_log off;
    }
}
```

## Maintenance

### Backup Database

```bash
# Manual backup
docker cp evm-hot-wallet:/app/data/wallet.db ./backups/wallet-$(date +%Y%m%d-%H%M%S).db

# Automated backup (cron)
0 */6 * * * docker cp evm-hot-wallet:/app/data/wallet.db /backups/wallet-$(date +\%Y\%m\%d-\%H\%M\%S).db
```

### Update Service

```bash
# Pull latest code
git pull origin main

# Rebuild and restart
docker-compose down
docker-compose build --no-cache
docker-compose up -d

# Verify
docker-compose logs -f
```

### View Logs

```bash
# Follow logs
docker-compose logs -f evm-hot-wallet

# Last 100 lines
docker-compose logs --tail=100 evm-hot-wallet

# Export logs
docker-compose logs --no-color > logs-$(date +%Y%m%d).txt
```

### Database Migration

If moving to a new server:

```bash
# On old server
docker cp evm-hot-wallet:/app/data/wallet.db ./wallet.db

# Copy to new server
scp wallet.db user@new-server:/path/to/evmhot/

# On new server
docker-compose up -d
docker cp wallet.db evm-hot-wallet:/app/data/wallet.db
docker-compose restart
```

## Troubleshooting

### Container won't start

```bash
# Check logs
docker-compose logs evm-hot-wallet

# Check environment variables
docker-compose config

# Verify .env file
cat .env
```

### Health check failing

```bash
# Check if service is responding
docker exec evm-hot-wallet curl http://localhost:3000/health

# Check network connectivity
docker-compose exec evm-hot-wallet ping -c 3 google.com
```

### Database issues

```bash
# Check database file
docker exec evm-hot-wallet ls -lh /app/data/

# Check disk space
docker exec evm-hot-wallet df -h /app/data

# Restore from backup
docker cp ./backups/wallet-20240101.db evm-hot-wallet:/app/data/wallet.db
docker-compose restart
```

### RPC connection issues

```bash
# Test RPC connectivity from container
docker exec evm-hot-wallet curl -X POST \
  -H "Content-Type: application/json" \
  --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
  $RPC_URL
```

## Monitoring

### Prometheus Metrics (Future Enhancement)

Add to `docker-compose.yml`:

```yaml
services:
  prometheus:
    image: prom/prometheus
    volumes:
      - ./prometheus.yml:/etc/prometheus/prometheus.yml
      - prometheus-data:/prometheus
    ports:
      - "9090:9090"

  grafana:
    image: grafana/grafana
    volumes:
      - grafana-data:/var/lib/grafana
    ports:
      - "3001:3000"
    depends_on:
      - prometheus
```

### Log Monitoring

Use Grafana Loki or ELK stack for centralized logging:

```yaml
services:
  loki:
    image: grafana/loki:2.9.0
    ports:
      - "3100:3100"
    volumes:
      - loki-data:/loki

  promtail:
    image: grafana/promtail:2.9.0
    volumes:
      - /var/lib/docker/containers:/var/lib/docker/containers:ro
      - ./promtail-config.yml:/etc/promtail/config.yml
    depends_on:
      - loki
```

## Cost Optimization

1. **RPC Costs**: Use your own node or choose cost-effective RPC providers
2. **Gas Optimization**: Consider implementing gas price strategies
3. **Resource Limits**: Add resource constraints to docker-compose.yml:

```yaml
services:
  evm-hot-wallet:
    # ... other config
    deploy:
      resources:
        limits:
          cpus: '1'
          memory: 512M
        reservations:
          cpus: '0.5'
          memory: 256M
```

## Support

For issues and questions:
- Check logs: `docker-compose logs -f`
- Review configuration: `.env` file
- Open an issue on GitHub

