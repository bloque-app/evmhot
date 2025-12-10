.PHONY: help build up down logs restart clean test fmt check health backup

help: ## Show this help message
	@echo 'Usage: make [target]'
	@echo ''
	@echo 'Available targets:'
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

build: ## Build the Docker image
	docker-compose build

up: ## Start the service
	docker-compose up -d

down: ## Stop the service
	docker-compose down

logs: ## View logs (follow)
	docker-compose logs -f evm-hot-wallet

restart: ## Restart the service
	docker-compose restart

clean: ## Stop and remove containers, volumes, and images
	docker-compose down -v --rmi local

test: ## Run tests locally
	cargo test

fmt: ## Format Rust code
	cargo fmt --all

check: ## Check Rust code compilation
	cargo check

clippy: ## Run clippy lints
	cargo clippy -- -D warnings

health: ## Check service health
	@curl -f http://localhost:3000/health && echo " - Service is healthy!" || echo " - Service is unhealthy!"

backup: ## Backup database
	@mkdir -p backups
	docker cp evm-hot-wallet:/app/data/wallet.db ./backups/wallet-$$(date +%Y%m%d-%H%M%S).db
	@echo "Database backed up to ./backups/"

status: ## Show container status
	docker-compose ps

shell: ## Open shell in container
	docker-compose exec evm-hot-wallet /bin/bash

dev: ## Run locally (not in Docker)
	cargo run

setup: ## Setup environment from template
	@if [ ! -f .env ]; then \
		cp env.docker.example .env; \
		echo "Created .env file from template. Please edit it with your configuration."; \
	else \
		echo ".env file already exists."; \
	fi

rebuild: down build up ## Rebuild and restart service

update: ## Pull latest code and rebuild
	git pull origin main
	$(MAKE) rebuild

prod-check: ## Check production readiness
	@echo "Checking production configuration..."
	@test -f .env || (echo "❌ .env file missing" && exit 1)
	@grep -q "YOUR_API_KEY" .env && echo "⚠️  Warning: .env contains placeholder values" || echo "✅ .env configured"
	@grep -q "test test test" .env && echo "⚠️  Warning: .env contains test mnemonics" || echo "✅ Mnemonics appear configured"
	@docker-compose config > /dev/null && echo "✅ docker-compose.yml is valid" || echo "❌ docker-compose.yml has errors"



