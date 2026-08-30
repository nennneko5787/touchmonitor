import Foundation

/// Mirrors the PC server's `protocol.rs`.
///
/// Every message is length-prefixed and little-endian on the wire:
/// `[ u32 length ][ u8 message_type ][ payload ... ]` where
/// `length = 1 (type byte) + payload size`.
enum StreamMessageType: UInt8 {
    case hello = 0x00
    case video = 0x01
    case info  = 0x02
    case touch = 0x03
    case ping  = 0x04
}

enum StreamProtocol {

    static let maxMessageLength = 16 * 1024 * 1024

    /// Serializes one `MSG_TOUCH` message, ready to write to the socket.
    ///
    /// `events` is an array of `(id, active, x01, y01)` where `x01`/`y01` are
    /// normalized 0..1 coordinates relative to the displayed desktop and
    /// `active == true` means the pointer is currently down.
    static func makeTouchMessage(events: [(id: UInt8, active: Bool, x: Float, y: Float)]) -> Data {
        var payload = Data()
        payload.append(UInt8(events.count))
        for e in events {
            payload.append(e.id)
            payload.append(e.active ? 1 : 0)
            var x = e.x
            var y = e.y
            withUnsafeBytes(of: &x) { payload.append(contentsOf: $0) }
            withUnsafeBytes(of: &y) { payload.append(contentsOf: $0) }
        }
        return frame(type: .touch, payload: payload)
    }

    /// Applies the `[u32 len][u8 type][payload]` framing.
    static func frame(type: StreamMessageType, payload: Data) -> Data {
        var out = Data()
        var len = UInt32(payload.count + 1).littleEndian
        withUnsafeBytes(of: &len) { out.append(contentsOf: $0) }
        out.append(type.rawValue)
        out.append(payload)
        return out
    }

    /// Reads one complete message from the buffer. Returns the first full
    /// `(type, payload)` if available, and consumes those bytes from `buffer`.
    /// Returns `nil` if a complete message is not yet buffered.
    ///
    /// On an invalid length, an error is thrown and the connection should close.
    ///
    /// Note: `Data.subdata(in:)`/`Data` subscripting trap (fatal) on out-of-range
    /// access, so we only touch the buffer after an explicit `buffer.count >= total`
    /// check and guard the envelope length against `maxMessageLength`.
    static func popMessage(from buffer: inout Data) throws -> (StreamMessageType, Data)? {
        guard buffer.count >= 5 else { return nil }

        let rawLen = buffer.withUnsafeBytes { $0.loadUnaligned(fromByteOffset: 0, as: UInt32.self) }
        let len = Int(UInt32(littleEndian: rawLen))

        // `len` counts the type byte + payload bytes (no length prefix).
        guard len >= 1, len <= maxMessageLength else {
            throw StreamError.badMessageLength
        }
        // Total bytes for this message on the wire: 4 (length) + len (type+payload).
        let total = len + 4
        guard buffer.count >= total else { return nil }

        let body = Data(buffer[4..<total])
        buffer.removeSubrange(0..<total)

        guard let rawType = body.first else {
            throw StreamError.badMessageLength
        }
        guard let type = StreamMessageType(rawValue: rawType) else {
            throw StreamError.unknownType(rawType)
        }
        if body.count == 1 {
            return (type, Data())
        }
        let payload = Data(body[1...])
        return (type, payload)
    }

    /// Parses a `MSG_VIDEO` payload into its parts.
    /// Layout: `[u8 keyframe][u32 width][u32 height][h264 annex-b]`.
    ///
    /// Safe: returns `nil` (and never traps) whenever the payload is too short.
    static func parseVideoPayload(_ payload: Data) -> (keyframe: Bool, width: UInt32, height: UInt32, h264: Data)? {
        let count = payload.count
        guard count >= 9 else { return nil }

        let keyframe = payload[payload.startIndex] != 0
        let width: UInt32 = payload.withUnsafeBytes {
            UInt32(littleEndian: $0.loadUnaligned(fromByteOffset: 1, as: UInt32.self))
        }
        let height: UInt32 = payload.withUnsafeBytes {
            UInt32(littleEndian: $0.loadUnaligned(fromByteOffset: 5, as: UInt32.self))
        }
        // 9 bytes header, remainder is the raw H.264 bitstream.
        guard count > 9 else { return (keyframe, width, height, Data()) }
        let h264 = Data(payload[9...])
        return (keyframe, width, height, h264)
    }
}

enum StreamError: LocalizedError {
    case badMessageLength
    case unknownType(UInt8)

    var errorDescription: String? {
        switch self {
        case .badMessageLength: return "Malformed message length"
        case .unknownType(let t): return "Unknown message type: \(t)"
        }
    }
}
