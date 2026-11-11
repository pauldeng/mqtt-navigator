#!/usr/bin/env bash
# Starts the test brokers and waits until each one answers an MQTT CONNECT.
# Usage: up.sh [mosquitto] [emqx] [hivemq]. No argument starts all three.
set -euo pipefail
cd "$(dirname "$0")"
services=("$@")
[ ${#services[@]} -gt 0 ] || services=(mosquitto emqx hivemq)
[ -f certs/server.crt ] || ./gen-certs.sh
docker compose up -d "${services[@]}"

# MQTT 3.1.1 CONNECT with an empty client ID. A broker that works answers CONNACK (20 02 00 00).
ready() { # port
  exec 3<>"/dev/tcp/127.0.0.1/$1" 2>/dev/null || return 1
  printf '\x10\x0c\x00\x04MQTT\x04\x02\x00\x3c\x00\x00' >&3
  local reply
  reply=$(timeout 1 head -c4 <&3 2>/dev/null | od -An -tx1 | tr -d ' \n' || true)
  exec 3>&-
  [ "$reply" = 20020000 ]
}

wait_for() { # service port seconds
  for _ in $(seq "$3"); do
    if ready "$2"; then echo "$1 ready."; return 0; fi
    sleep 1
  done
  echo "$1 did not become ready in $3 s." >&2
  docker compose logs --tail 30 "$1" >&2
  return 1
}

for s in "${services[@]}"; do
  case "$s" in
    mosquitto) wait_for mosquitto 18830 30 ;;
    emqx) wait_for emqx 18840 90 ;;
    hivemq) wait_for hivemq 18850 90 ;;
    *) echo "Unknown service: $s" >&2; exit 2 ;;
  esac
done
