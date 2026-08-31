# Verification Specification

## Build identity

Every test artifact must print the exact source commit (short SHA, at least 10 characters). A build showing an older SHA is invalid evidence. Local `cargo run --release` must rebuild when Git HEAD changes without requiring `cargo clean`.

## Automated checks

From `pc-server`:

```text
cargo check --offline
cargo test --offline
```

The server build must have no warnings caused by the changed code. `cargo check` success alone does not prove capture, encoding, or networking.

## Windows manual test

1. Stop any old `touchmonitor-server.exe` process.
2. Run `cargo run --release -- --monitor 1`.
3. Confirm the printed commit matches the source under test.
4. Connect from the iPad.
5. Save the complete server log from startup through first frame and disconnect.
6. Confirm the log contains a non-zero encoded frame and no MFT timeout.

## iPad manual test

1. Open the in-app connection log.
2. Connect through Bonjour with no IP entry.
3. Confirm service resolution, TCP connection, hello, UDP packet receipt, decoder initialization, and visible video.
4. Test center, four corners, a drag, and a two-finger gesture.
5. Disconnect and reconnect twice.

## USB manual test

1. Connect the iPad to Windows with USB and ensure the device is trusted/paired.
2. Ensure `iproxy.exe` from libusbmuxd is installed and available on `PATH`.
3. Run `cargo run --release -- --usb --monitor 1`.
4. Confirm the server log reports the usbmuxd/iproxy mapping and an established iOS connection.
5. In the iPad app, choose `Connect over USB` and confirm the USB listener, hello, first TCP video frame, decoder initialization, and visible video in the log.
6. Test center, four corners, a drag, and a two-finger gesture.
7. Disconnect and reconnect twice without restarting the server or app.

USB acceptance requires at least one non-zero `MSG_VIDEO` frame and visible video; a successful usbmuxd tunnel with no decoded frame is a failed test.

## H.264-specific acceptance

For each input frame the encoder must obey the selected MFT's processing model:

- Async MFT: register `BeginGetEvent`, call `EndGetEvent` in the callback, process exactly one input per `METransformNeedInput`, and call `ProcessOutput` only after `METransformHaveOutput`.
- Sync MFT: do not wait for async events; use the synchronous `ProcessInput`/`ProcessOutput` contract.

The implementation must identify which model is active instead of assuming it from the successful COM cast. A timeout waiting for event 602 is a failed test and must report the selected MFT and async attribute.
