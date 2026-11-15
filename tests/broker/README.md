# Broker test matrix

These files start real brokers in Docker: Mosquitto 2.x, EMQX and HiveMQ CE. The tests in `crates/navigator-core/tests/broker_matrix.rs` use Mosquitto. The tests in `broker_compat.rs` run the same checks against all three brokers. The soak test (`soak.rs`) uses Mosquitto.

## Run

```bash
tests/broker/up.sh && cargo test -p navigator-core --features broker-tests --test broker_matrix --test broker_compat && tests/broker/down.sh
```

`up.sh` takes service names (`mosquitto`, `emqx`, `hivemq`). With no name, it starts all three. `mosquitto` starts two Mosquitto services: the main one and `mosquitto-limits`.

The soak test runs 1 h by default (`SOAK_SECS`, `SOAK_RATE`). It restarts the broker 6 times:

```bash
tests/broker/up.sh mosquitto && SOAK_SECS=600 cargo test --release -p navigator-core --features broker-tests --test soak -- --ignored --nocapture && tests/broker/down.sh
```

You need Docker with Compose, and the `openssl` command.
`broker_compat` has one test for each broker that restarts it. A `RwLock` in that file keeps it apart from the other tests of the file.

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
| 18838 | Secure WebSocket, client certificate required |

Other brokers (the ports are on `127.0.0.1`):

| Port  | Listener                      |
| ----- | ----------------------------- |
| 18860 | Second Mosquitto (`mosquitto-limits.conf`): TCP, anonymous. Server Keep Alive 5 s (`max_keepalive`), Receive Maximum 2 (`max_inflight_messages`), Maximum Packet Size 4096 (`max_packet_size`). Mosquitto sets these for the whole broker, so they need their own service. |
| 18840 | EMQX, TCP, anonymous          |
| 18841 | EMQX, WebSocket (path `/mqtt`) |
| 18850 | HiveMQ CE, TCP, anonymous     |
| 18851 | HiveMQ CE, WebSocket (path `/mqtt`) |

## Files

- `compose.yml`: the broker images (pinned by digest) and the port mapping.
- `hivemq-config.xml`: the HiveMQ listeners.
- `mosquitto.conf`: the listeners. `mosquitto-limits.conf`: the second Mosquitto with the global limits.
- `gen-certs.sh`: makes a test CA, server and client certificates, and the password file in `certs/`. It also makes the bad cases: an expired certificate, a certificate for another host name, and a client key that does not match its certificate. `up.sh` runs it when `certs/` is empty.
- `up.sh`: starts the brokers. It waits for an MQTT CONNACK (30 s for Mosquitto, 90 s for the others). It makes a real connect on every listener: Mosquitto 18830 to 18838 and 18860 (the two bad-certificate listeners without verification), EMQX 18840 and 18841, HiveMQ 18850 and 18851. It needs `openssl` for the TLS ones.
- `down.sh`: stops and removes the brokers.

`certs/` is in `.gitignore`. The keys are for tests only. Do not use them anywhere else.
