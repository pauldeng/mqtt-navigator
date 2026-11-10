# Broker test matrix

These files start a real Mosquitto 2.x broker in Docker. The tests in `crates/navigator-core/tests/broker_matrix.rs` connect to it.

## Run

```bash
tests/broker/up.sh && cargo test -p navigator-core --features broker-tests --test broker_matrix -- --test-threads=1 && tests/broker/down.sh
```

You need Docker with Compose, and the `openssl` command.
Use `--test-threads=1`. One test restarts the broker.

## Listeners

All listeners are on `127.0.0.1`. The ports do not clash with a local broker on 1883.

| Port  | Listener                                  |
| ----- | ----------------------------------------- |
| 18830 | TCP, anonymous                            |
| 18831 | TCP, user `test`, password `test-pass`    |
| 18832 | TLS                                       |
| 18833 | TLS, client certificate required          |
| 18834 | WebSocket (path `/mqtt`)                  |
| 18835 | Secure WebSocket                          |
| 18836 | TLS, expired server certificate           |
| 18837 | TLS, server certificate for `wrong.example` |

## Files

- `compose.yml`: the broker image (pinned) and the port mapping.
- `mosquitto.conf`: the listeners.
- `gen-certs.sh`: makes a test CA, server and client certificates, and the password file in `certs/`. It also makes the bad cases: an expired certificate, a certificate for another host name, and a client key that does not match its certificate. `up.sh` runs it when `certs/` is empty.
- `up.sh`: starts the broker. It waits at most 30 s for an MQTT CONNACK.
- `down.sh`: stops and removes the broker.

`certs/` is in `.gitignore`. The keys are for tests only. Do not use them anywhere else.
