#!/bin/bash
set -e

# Write config from Fly secret
if [ -n "$CONFIG_TOML" ]; then
    echo "$CONFIG_TOML" > /app/config.toml
fi

# Write PEM key from Fly secret
if [ -n "$KALSHI_PRIVATE_KEY" ]; then
    echo "$KALSHI_PRIVATE_KEY" > /app/kalshi_private_key.pem
    chmod 600 /app/kalshi_private_key.pem
fi

exec /app/sports-betting /app/config.toml
