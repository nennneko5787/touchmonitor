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
    static func popMessage(from buffer: inout Data) throws -> (StreamMessageType, Data)? {
        guard buffer.count >= 5 else { return nil }
        let len = buffer.withUnsafeBytes { $0.loadUnaligned(fromByteOffset: 0, as: UInt32.self) }
        let lenLE = UInt32(littleEndian: len)
        let total = Int(lenLE) + 4
        guard lenLE >= 1, Int(lenLE) <= maxMessageLength else {
            throw StreamError.badMessageLength
        }
        guard buffer.count >= total else { return nil }
        let body = buffer.subdata(in: 4..<total)
        buffer.removeFirst(total)
        guard let rawType = body.first else {
            throw StreamError.badMessageLength
        }
        guard let type = StreamMessageType(rawValue: rawType) else {
            throw StreamError.unknownType(rawType)
        }
        let payload = body.dropFirst()
        return (type, Data(payload))
    }

    /// Parses a `MSG_VIDEO` payload into its parts.
    /// Layout: `[u8 keyframe][u32 width][u32 height][h264 annex-b]`.
    static func parseVideoPayload(_ payload: Data) -> (keyframe: Bool, width: UInt32, height: UInt32, h264: Data)? {
        guard payload.count >= 9 else { return nil }
        let keyframe = payload[payload.startIndex] != 0
        let width = payload.withUnsafeBytes {
            UInt32(littleEndian: $0.loadUnaligned(fromByteOffset: 1, as: UInt32.self))
        }
        let height = payload.withUnsafeBytes {
            UInt32(littleEndian: $0.loadUnaligned(fromByteOffset: 5, as: UInt32.self))
        }
        let h264 = payload.subdata(in: payload.startIndex + 9 ..< payload.endIndex)
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
