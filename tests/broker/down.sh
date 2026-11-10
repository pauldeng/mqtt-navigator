#!/usr/bin/env bash
# Stops and removes the test broker.
cd "$(dirname "$0")"
docker compose down -v
