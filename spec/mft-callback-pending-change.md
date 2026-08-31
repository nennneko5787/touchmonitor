# Pending MFT Callback Change

## Status

This document describes the uncommitted working-tree change currently present in `pc-server/src/encoder_mft.rs`. It is intentionally documented separately from the committed implementation status. The change has not passed the Windows end-to-end acceptance test and must not be described as a fix.

## Exact intended change

The pending diff changes three things:

1. It calls `IMFMediaEventGenerator::BeginGetEvent` immediately after `MFT_MESSAGE_NOTIFY_START_OF_STREAM`.
2. It changes `wait_for_event` from direct `GetEvent(MF_EVENT_FLAG_NO_WAIT)` polling to waiting on `EventState`/`Condvar` populated by `IMFAsyncCallback::Invoke`.
3. It re-arms `BeginGetEvent` after every received event and uses the callback queue for `METransformHaveOutput`.

The pending diff does not change capture, NV12 conversion, network transport, or H.264 packet formatting.

## Reason for the change

The previous committed build `833fa54` removed `BeginGetEvent` and attempted to read the event queue directly, but still timed out waiting for event 602. The callback path is being restored because the Windows asynchronous MFT contract requires `BeginGetEvent`/`EndGetEvent` for out-of-band transform events.

## Required verification before commit

Before committing this diff, run:

```text
cargo check --offline
cargo run --release -- --monitor 1
```

On the target Windows machine, verify all of the following:

- the printed build commit identifies the exact tested source;
- the selected MFT identity and `MF_TRANSFORM_ASYNC` value are logged;
- `IMFAsyncCallback::Invoke` is entered for the first event;
- `METransformNeedInput` is received before the first `ProcessInput`;
- `METransformHaveOutput` is received after `ProcessInput`;
- at least one encoded frame has non-zero length;
- the iPad renders video;
- no `timed out waiting for MFT event 602` occurs.

If `Invoke` is not entered, inspect COM apartment/thread initialization and callback lifetime before changing the frame pipeline. If the selected MFT reports `MF_TRANSFORM_ASYNC = 0`, do not wait for events; implement the synchronous MFT path instead.

## Commit discipline

Do not commit this pending change merely because compilation succeeds. Commit only after the Windows test above passes, then update `implementation-status.md` with the result and the exact tested commit.
