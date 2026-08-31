# TouchMonitor Specifications

This directory is the source of truth for implementation and review. Code changes must satisfy the acceptance criteria here before being called complete.

## Documents

- `system-spec.md`: product boundaries, transport, startup, and end-to-end acceptance criteria.
- `mft-encoder-spec.md`: H.264 encoder requirements and state machine.
- `network-transport-spec.md`: Bonjour discovery, TCP control, and UDP video behavior.
- `verification-spec.md`: reproducible test procedure and evidence required from each build.
- `implementation-status.md`: current known state, failed approaches, and handoff notes.

## Specification-driven workflow

1. Read this README and `implementation-status.md` before editing code.
2. Select one requirement and state its acceptance test.
3. Implement the smallest change that satisfies that test.
4. Run the listed automated checks and record the exact commit ID in the application/server output.
5. Do not claim completion until the relevant manual test passes on Windows and iPad.

A timeout, disconnect, black screen, or stale build artifact is a failed acceptance test, even if compilation succeeds.
