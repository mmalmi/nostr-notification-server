# Nostr Notification Server

A server that sends web push notifications and calls webhooks when Nostr events match user-defined filters.

Built on Rust, Heed (LMDB) and Flatbuffers.

## Features
- Receives events from Nostr relays and REST API
- Receives web hook and web push subscriptions over REST API
- Sends notifications when events match subscription filters

## Setup

1. Install dependencies:
```bash
cargo build
```

2. Configure settings in [config/default.toml](config/default.toml)

3. Run server:
```bash
cargo run
```

## Configuration
Edit [config/default.toml](config/default.toml):
```toml
# Server settings (required for NIP-98 auth)
http_port = 3030
base_url = "https://example.com"  # Set to your public address where the application will be accessed

# Nostr relays to connect to
relays = [
    "wss://relay.damus.io",
    "wss://nos.lol"
]

# Database size limit in bytes
db_map_size = 1073741824  # 1GB
...
```

Configuration can also be set via environment variables with the `NNS_` prefix:
```bash
NNS_HTTP_PORT=8080
NNS_BASE_URL=https://example.com
NNS_DB_MAP_SIZE=2147483648
NNS_RELAYS='["wss://relay1.com","wss://relay2.com"]'
```

## API (Default port: 3030)

### Get VAPID public key for web push
```bash
curl http://localhost:3030/info
```

### Manage Subscriptions
Requires [NIP-98 HTTP Auth](https://github.com/nostr-protocol/nips/blob/master/98.md)
```bash
# Get subscription
curl http://localhost:3030/subscriptions/<pubkey> -H "Authorization: <nostr-auth>"

# Create/update subscription
curl -X POST http://localhost:3030/subscriptions/<pubkey> \
  -H "Authorization: <nostr-auth>" \
  -H "Content-Type: application/json" \
  -d '{"webhooks":["https://example.com/hook"], "filter":{"kinds":[1]}}'

# Delete subscription
curl -X DELETE http://localhost:3030/subscriptions/<pubkey> -H "Authorization: <nostr-auth>"
```

### Post Event
```bash
curl -X POST http://localhost:3030/events \
  -H "Content-Type: application/json" \
  -d '{"id":"...","pubkey":"...","kind":1,...}'
```

## Development

Run tests:
```bash
RUST_LOG=debug cargo test
```

Watch tests:
```bash
RUST_LOG=debug cargo watch -x test
```
Update schema:
```bash
flatc --rust -o src/schema/ src/schema/subscription.fbs
```
