# Network and Discovery Specification

## Service discovery

- Service type: `_touchmonitor._tcp`.
- Domain: `local`.
- Instance name: `TouchMonitor` (must be collision-safe if another instance exists).
- The SRV target must resolve to an A/AAAA record reachable from the iPad.
- The client must use the resolved endpoint, never a cached or hard-coded IP.
- Advertisement failure is fatal and must include the native error code in the log.

## Connections

- TCP control connection: server listens on the advertised service port, default `5666`.
- TCP uses `TCP_NODELAY` and carries hello/control/touch messages.
- The hello message contains the UDP video port and monitor dimensions before video decoding begins.
- UDP video uses the resolved peer address and the port from hello.
- TCP disconnect must stop the writer loop and unregister the UDP client.

## Required diagnostics

The server must log, in order:

1. build commit;
2. advertisement instance, target hostname, and resolved address;
3. accepted TCP peer;
4. selected monitor dimensions and origin;
5. encoder type (hardware/software), async mode, and negotiated format;
6. UDP registration;
7. first encoded frame size and keyframe status;
8. disconnect reason.

The iOS app must log discovery, resolution, TCP connect, hello receipt, UDP registration, first packet, decoder initialization, and decoder errors.

## Failure rules

- Bonjour discovery failure must not silently fall back to an IP.
- A TCP connection without an encoded frame is considered failed.
- A UDP registration without received packets is considered failed.
- The client must show the actual error and build commit in its in-app log viewer.
