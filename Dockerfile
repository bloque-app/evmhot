# Build stage
FROM rust:1.75-slim as builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY src ./src

# Build the application in release mode
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

WORKDIR /app

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Copy the built binary from the builder stage
COPY --from=builder /app/target/release/evm_hot_wallet /usr/local/bin/evm_hot_wallet

# Create a directory for the database
RUN mkdir -p /app/data

# Set environment variables for database location
ENV DATABASE_URL=/app/data/wallet.db

# Expose the API port
EXPOSE 3000

# Run the binary
CMD ["evm_hot_wallet"]

