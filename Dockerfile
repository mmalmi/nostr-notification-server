# Build stage
FROM rust:1.82-slim-bullseye as builder

# Install OpenSSL for VAPID key generation
RUN apt-get update && apt-get install -y pkg-config libssl-dev

WORKDIR /usr/src/app

# Copy only manifests first to cache dependencies
COPY Cargo.toml Cargo.lock ./

# Create a dummy main.rs to build dependencies
RUN mkdir src \
    && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

# Now copy the real source code
COPY src/ src/
COPY config/ config/
COPY tests/ tests/

# Build the actual application
RUN cargo build --release

# Runtime stage
FROM debian:bullseye-slim

# Install OpenSSL for runtime
RUN apt-get update && apt-get install -y openssl ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy binary from builder
COPY --from=builder /usr/src/app/target/release/nostr-notification-server .

# Set environment variables
ENV NNS_DB_PATH=/app/db
ENV RUST_LOG=info

# Expose HTTP port
EXPOSE 3030

# Mount points for persistent data
VOLUME ["/app/db", "/app/config"]

CMD ["./nostr-notification-server"]