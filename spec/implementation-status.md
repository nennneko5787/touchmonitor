# Implementation Status and Handoff Notes

## Confirmed working

- Windows Graphics Capture delivers frames. Logs show `WGC FrameArrived`, extraction, and `1920x1080` frames.
- Bonjour advertisement is visible from Windows tools.
- iPad-to-PC TCP connection succeeds.
- UDP video client registration succeeds.
- The server prints an embedded Git commit when built through Cargo.
- Warning cleanup was added in commit `c15cc9f`.

## Reproducible failure

With local commit `238c549` and later `833fa54`, the server reaches encoder initialization and UDP registration, then disconnects with:

```text
timed out waiting for MFT event 602
```

Event 602 is `METransformNeedInput`. The failure occurs after capture and network setup; it is not a Bonjour or TCP problem.

## Known implementation history

- `90f738a`: changed async input/output flow and removed `GetEvent` output polling.
- `40dbe81`: added `MFT_ENUM_FLAG_ASYNCMFT` to hardware encoder enumeration.
- `09f6c98`: made Cargo rebuild the embedded commit when Git changes.
- `18c7a74`: changed callback registration order.
- `833fa54`: temporarily replaced callback waiting with direct event-queue polling; this still timed out.
- `238c549`: initialized COM on the client worker thread; this still timed out.

## Next implementation requirement

Do not make another speculative encoder change without first recording the selected MFT identity and its `MF_TRANSFORM_ASYNC` attribute. The implementation must support both async and sync MFTs, or explicitly fail during initialization with a diagnostic that identifies why no supported encoder exists.

For an async MFT, follow the Microsoft contract: unlock the MFT, set both media types, send `BEGIN_STREAMING` and `START_OF_STREAM`, register `BeginGetEvent`, call `EndGetEvent` from a live `IMFAsyncCallback`, re-arm the callback after every event, and pair each `NeedInput`/`HaveOutput` event with exactly one transform call.

Do not treat these as success:

- encoder initialization alone;
- a TCP client connection alone;
- UDP registration alone;
- a non-zero process exit caused by Ctrl+C;
- a build whose printed commit does not match the source being tested.
