//! Shared wire protocol between the PC server and the iOS client.
//!
//! Every message is length-prefixed and little-endian on the wire:
//!
//! ```text
//! [ u32 length ][ u8  message_type ][ payload ... ]
//! ```
//!
//! where `length` = 1 (the type byte) + payload size.

use std::io;

/// Handshake: sent by the server right after a client connects.
pub const MSG_HELLO: u8 = 0x00;
/// Server -> client: one encoded video access unit (H.264 Annex-B).
pub const MSG_VIDEO: u8 = 0x01;
/// Server -> client: stream status / error string.
pub const MSG_INFO: u8 = 0x02;
/// Client -> server: a batch of touch events.
pub const MSG_TOUCH: u8 = 0x03;
/// Client -> server: keep-alive / ping.
pub const MSG_PING: u8 = 0x04;
/// TCP control handshake: payload is `[u16 udp_port]`.

pub const MAX_MESSAGE: usize = 16 * 1024 * 1024;

pub fn make_hello(udp_port: u16) -> Vec<u8> {
    let mut out = Vec::new();
    write_message(&mut out, MSG_HELLO, &udp_port.to_le_bytes());
    out
}

/// Writes a single framed message into `out`:
/// `[u32 len][u8 type][payload]` where `len = payload.len() + 1`.
pub fn write_message(out: &mut Vec<u8>, msg_type: u8, payload: &[u8]) {
    let len = (payload.len() + 1) as u32;
    out.extend_from_slice(&len.to_le_bytes());
    out.push(msg_type);
    out.extend_from_slice(payload);
}

/// Reads exactly one framed message from `reader`.
/// Returns `(msg_type, payload)`.
pub fn read_message<R: io::Read>(reader: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len < 1 || len > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad message length"));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    let msg_type = body[0];
    let payload = body[1..].to_vec();
    Ok((msg_type, payload))
}

/// The video frame header that precedes each `MSG_VIDEO` payload.
/// Payload layout after the type byte:
/// ```text
/// [u8  keyframe][u32 width][u32 height][h264 annex-b payload]
/// ```
pub fn make_video_payload(keyframe: bool, width: u32, height: u32, h264: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(1 + 8 + h264.len());
    p.push(keyframe as u8);
    p.extend_from_slice(&width.to_le_bytes());
    p.extend_from_slice(&height.to_le_bytes());
    p.extend_from_slice(h264);
    p
}

/// Encodes interactive touch events (per event) into a `MSG_TOUCH` payload and returns a
/// freshly-framed message ready to send.
///
/// `events` is a slice of `(touch_id, active, x01, y01)` where `x01`/`y01` are normalized
/// 0..1 coordinates relative to the displayed desktop, and `active == true` means the pointer
/// is currently down (touch contact).
pub fn encode_touch(events: &[(u8, bool, f32, f32)]) -> Vec<u8> {
    let mut p = Vec::with_capacity(1 + events.len() * 10);
    p.push(events.len() as u8);
    for (id, active, x, y) in events {
        p.push(*id);
        p.push(*active as u8);
        p.extend_from_slice(&x.to_le_bytes());
        p.extend_from_slice(&y.to_le_bytes());
    }
    p
}

/// Serialize one touch message (framed, ready to write to the socket).
pub fn make_touch_message(events: &[(u8, bool, f32, f32)]) -> Vec<u8> {
    let payload = encode_touch(events);
    let mut out = Vec::new();
    write_message(&mut out, MSG_TOUCH, &payload);
    out
}

/// Deserialize a `MSG_TOUCH` payload produced by `encode_touch`.
pub fn decode_touch(payload: &[u8]) -> Option<Vec<(u8, bool, f32, f32)>> {
    if payload.len() < 1 {
        return None;
    }
    let count = payload[0] as usize;
    if payload.len() < 1 + count * 10 {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let base = 1 + i * 10;
        let id = payload[base];
        let active = payload[base + 1] != 0;
        let x = f32::from_le_bytes([payload[base + 2], payload[base + 3], payload[base + 4], payload[base + 5]]);
        let y = f32::from_le_bytes([payload[base + 6], payload[base + 7], payload[base + 8], payload[base + 9]]);
        out.push((id, active, x, y));
    }
    Some(out)
}
