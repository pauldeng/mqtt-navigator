#!/usr/bin/env bash
# Starts the test brokers and waits until each one answers an MQTT CONNECT.
# A real connect on every listener: Mosquitto (TCP, auth, TLS, mTLS, WS, WSS, WSS with mTLS, expired
# and wrong-host certificates, the last two without verification), the second Mosquitto with the
# global limits (TCP), EMQX (TCP, WS) and HiveMQ (TCP, WS).
# Usage: up.sh [mosquitto] [emqx] [hivemq]. No argument starts all three. "mosquitto" starts both
# Mosquitto services.
set -euo pipefail
cd "$(dirname "$0")"
services=("$@")
[ ${#services[@]} -gt 0 ] || services=(mosquitto emqx hivemq)
[ -f certs/server.crt ] || ./gen-certs.sh
compose_services=()
for s in "${services[@]}"; do
  compose_services+=("$s")
  [ "$s" != mosquitto ] || compose_services+=(mosquitto-limits)
done
docker compose up -d "${compose_services[@]}"

# MQTT 3.1.1 CONNECT with an empty client ID. A broker that works answers CONNACK (20 02 00 00).
CONNECT='\x10\x0c\x00\x04MQTT\x04\x02\x00\x3c\x00\x00'
# The same with keep alive 5 s: the second Mosquitto (max_keepalive 5) refuses a MQTT 3.1.1 client
# with a longer one.
CONNECT_KA5='\x10\x0c\x00\x04MQTT\x04\x02\x00\x05\x00\x00'
# The same with user name "test" and password "test-pass".
CONNECT_AUTH='\x10\x1d\x00\x04MQTT\x04\xc2\x00\x3c\x00\x00\x00\x04test\x00\x09test-pass'
# WebSocket upgrade, and one binary frame with the CONNECT (mask key 0, so the bytes stay as they are).
UPGRADE='GET /mqtt HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: mqtt\r\n\r\n'
WS_CONNECT='\x82\x8e\x00\x00\x00\x00\x10\x0c\x00\x04MQTT\x04\x02\x00\x3c\x00\x00'
CONNACK=20020000
WS_CONNACK=820420020000

hex() { od -An -tx1 | tr -d ' \n'; }

# plain <port> <bytes>...: sends each part (printf format) with a pause, prints the reply as hex.
plain() {
  local port=$1; shift
  exec 3<>"/dev/tcp/127.0.0.1/$port" 2>/dev/null || return 0
  local part
  for part in "$@"; do printf "$part" >&3; sleep 0.3; done
  timeout 1 cat <&3 2>/dev/null | hex || true
  exec 3>&-
}

# tls <port> <bytes>... [-- s_client options]: the same over TLS, with the test CA and host name.
tls() {
  local port=$1; shift
  local parts=()
  while [ $# -gt 0 ] && [ "$1" != -- ]; do parts+=("$1"); shift; done
  [ "${1:-}" != -- ] || shift
  {
    local part
    for part in "${parts[@]}"; do printf "$part"; sleep 0.3; done
    sleep 1
  } | timeout 5 openssl s_client -quiet -connect "127.0.0.1:$port" -servername localhost \
      -CAfile certs/ca.crt -verify_return_error -verify_hostname localhost "$@" 2>/dev/null |
    hex || true
}

# tls_unverified <port> <bytes>: the same, for the listeners with a bad server certificate (expired, wrong host
# name). The probe must not verify the certificate: it proves only that the listener answers.
tls_unverified() {
  local port=$1 bytes=$2
  {
    printf "$bytes"
    sleep 1
  } | timeout 5 openssl s_client -quiet -connect "127.0.0.1:$port" -servername localhost 2>/dev/null |
    hex || true
}

# Each check prints the reply as hex. The reply must have the CONNACK in it.
check() { # port
  case "$1" in
    18830) [ "$(plain 18830 "$CONNECT")" = "$CONNACK" ] ;;
    18860) [ "$(plain 18860 "$CONNECT_KA5")" = "$CONNACK" ] ;;
    18831) [ "$(plain 18831 "$CONNECT_AUTH")" = "$CONNACK" ] ;;
    18832) [ "$(tls 18832 "$CONNECT")" = "$CONNACK" ] ;;
    18833) [ "$(tls 18833 "$CONNECT" -- -cert certs/client.crt -key certs/client.key)" = "$CONNACK" ] ;;
    18834) [[ "$(plain 18834 "$UPGRADE" "$WS_CONNECT")" == *"$WS_CONNACK" ]] ;;
    18835) [[ "$(tls 18835 "$UPGRADE" "$WS_CONNECT")" == *"$WS_CONNACK" ]] ;;
    18838) [[ "$(tls 18838 "$UPGRADE" "$WS_CONNECT" -- -cert certs/client.crt -key certs/client.key)" == *"$WS_CONNACK" ]] ;;
    18836 | 18837) [ "$(tls_unverified "$1" "$CONNECT")" = "$CONNACK" ] ;;
    18841 | 18851) [[ "$(plain "$1" "$UPGRADE" "$WS_CONNECT")" == *"$WS_CONNACK" ]] ;;
    18840 | 18850) [ "$(plain "$1" "$CONNECT")" = "$CONNACK" ] ;;
  esac
}

wait_for() { # service seconds port...
  local name=$1 seconds=$2; shift 2
  local port
  for port in "$@"; do
    local ok=0
    for _ in $(seq "$seconds"); do
      if check "$port"; then ok=1; break; fi
      sleep 1
    done
    if [ "$ok" != 1 ]; then
      echo "$name: no CONNACK on port $port in $seconds s." >&2
      docker compose logs --tail 30 "$name" >&2
      return 1
    fi
    echo "$name: port $port answers CONNACK."
  done
  echo "$name ready."
}

for s in "${services[@]}"; do
  case "$s" in
    mosquitto) wait_for mosquitto 30 18830 18831 18832 18833 18834 18835 18836 18837 18838 18860 ;;
    emqx) wait_for emqx 90 18840 18841 ;;
    hivemq) wait_for hivemq 90 18850 18851 ;;
    *) echo "Unknown service: $s" >&2; exit 2 ;;
  esac
done
