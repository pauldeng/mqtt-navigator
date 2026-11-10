# Vendored crates

## ms-mqtt-client

- Source: crates.io `ms-mqtt-client` 0.1.0 (MIT, Microsoft), copied without changes in the first commit.
- The second commit adds a local patch. See `git log -- vendor/ms-mqtt-client`.
- Reason: the crate does not limit incoming packet size, and its incoming-publish queue has no bound. See ADR 0002.
- Upgrade: copy the new upstream release, apply the patch again, and run the tests. Remove the fork when upstream fixes both issues.
