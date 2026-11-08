# QAQH Harmony Native Client

This module exposes the Rust Ringing V2 remote client to ArkTS. The Rust client
owns the TLS connection, lease, command envelope, and session stream.

## Build the arm64 HAR

Install the OpenHarmony native SDK and set `OHOS_NDK_HOME` to the directory
containing `native/llvm` and `native/sysroot`. Run from the backend workspace root.

```sh
rustup target add aarch64-unknown-linux-ohos
cargo install --locked ohrs --version 1.5.0

CARGO_PROFILE_DEV_DEBUG=0 ohrs build -p qaqh-harmony-napi --arch arm64 \
  --target-dir "$PWD/target/harmony-build" --dist "$PWD/target/harmony-dist" \
  -- --locked --jobs 2

mkdir -p target/harmony-har/package
cp crates/qaqh-harmony-napi/package/* target/harmony-har/package/
(
  cd target/harmony-har
  ohrs artifact --no-workspace --dist ../harmony-dist --name qaqh-native-client
)
```

Use `target/harmony-har/qaqh-native-client.har` as a local dependency in the
Harmony app and import the API from `@qaqh/native-client`. The generated
`target/harmony-dist/index.d.ts` contains the full ArkTS interface.
For a release build, pass `--release` to `ohrs build`.

## Device smoke

1. Call `pairDevice` with the endpoint and TLS fingerprint from the daemon's
   pairing QR. Store the returned device token using the app's credential store.
2. Call `connectClient` with the device token and fingerprint. Pick a session
   from `sessionList()` and call `startSession(sessionId)`. Check that the
   returned attach receipt was accepted.
3. Consume `nextUpdate()` in one loop. On `open`, fetch `bootstrap(sessionId)`
   before consuming more updates. If the snapshot and stream epochs differ,
   wait for the next `open`. Otherwise apply subsequent frames for that session
   with `seq >= nextSeq`, including the equal boundary. Both sequence values
   are decimal strings. On `reset`, wait for the next `open` and take a fresh
   snapshot. On `lagged`, close and reopen the client to establish a fresh
   baseline.
4. Submit a canonical `conversation.send_message` using `sendCommand` with a
   session target and check the receipt and event stream. Disconnect and
   reconnect the network to verify recovery.
5. Call `stopSession` when leaving a session. Filter queued updates by the
   currently selected session. Call `close()` when disposing the client.
   `nextUpdate()` resolves without an update when the client closes.
