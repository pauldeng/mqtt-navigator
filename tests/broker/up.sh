#!/usr/bin/env bash
# Starts the test broker and waits until it answers an MQTT CONNECT (30 s at most).
set -euo pipefail
cd "$(dirname "$0")"
[ -f certs/server.crt ] || ./gen-certs.sh
docker compose up -d
# MQTT 3.1.1 CONNECT with an empty client ID. A broker that works answers CONNACK (20 02 00 00).
for _ in $(seq 30); do
  if exec 3<>/dev/tcp/127.0.0.1/18830 2>/dev/null; then
    printf '\x10\x0c\x00\x04MQTT\x04\x02\x00\x3c\x00\x00' >&3
    reply=$(timeout 1 head -c4 <&3 2>/dev/null | od -An -tx1 | tr -d ' \n' || true)
    exec 3>&-
    if [ "$reply" = 20020000 ]; then echo "Broker ready."; exit 0; fi
  fi
  sleep 1
done
echo "The broker did not become ready in 30 s." >&2
docker compose logs --tail 30 >&2
exit 1
