# TouchMonitor System Specification

## Purpose

Show a selected Windows monitor on an iPad and send iPad multi-touch input back to Windows with low latency. The system must work without manually entering an IP address.

## Scope

The PC server owns monitor capture, H.264 encoding, service advertisement, video transport, and Windows touch injection. The iOS app owns Bonjour discovery, connection establishment, H.264 decoding, display, and touch event generation.

## Required behavior

1. On startup the PC server prints its build commit and advertises `_touchmonitor._tcp` using the configured control port.
2. The iOS app discovers the service through Bonjour and resolves the advertised hostname. It must not contain a hard-coded PC IP address.
3. The app establishes one TCP control connection and receives the video UDP port in the hello message.
4. The server captures the requested monitor using Windows Graphics Capture.
5. The server encodes every captured frame as decodable H.264 without a black screen.
6. The app displays the first keyframe and subsequent frames.
7. Touch events use normalized coordinates and persistent finger IDs; the server injects them into the selected monitor.
8. A disconnect releases the capture, encoder, sockets, and touch state without requiring a reboot.

## Non-goals

- Manual IP entry as the normal connection path.
- Requiring a special Ethernet adapter for the baseline Wi-Fi path.
- Calling a successful TCP connection a successful video connection.

## End-to-end acceptance criteria

The feature is complete only when all are true on the target Windows machine and iPad:

- The server starts without an address/port conflict.
- Bonjour discovery resolves and TCP connects.
- The server logs at least one encoded frame with a non-zero byte count.
- The iPad renders the desktop image for 30 seconds with no persistent black frame.
- Touches at all four corners and the center land within the agreed tolerance.
- Reconnect works twice without restarting either device.
- The server exits without an encoder timeout or unhandled process error.
