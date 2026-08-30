import Foundation
import Network

/// Owns the TCP connection to the PC server and pumps protocol messages.
///
/// * A receive loop accumulates bytes and pops framed messages (`StreamProtocol`).
/// * `MSG_VIDEO` payloads are forwarded to the `H264Decoder`.
/// * `MSG_INFO` strings are surfaced via `onStatus`.
/// * Multitouch is sent with `sendTouches(_:)`.
class NetworkClient {

    enum State {
        case idle
        case connecting
        case connected
        case failed(String)
        case disconnected
    }

    private(set) var state: State = .idle {
        didSet { onStateChange?(state) }
    }

    var onStateChange: ((State) -> Void)?
    var onStatus: ((String) -> Void)?
    var onVideoMeta: ((UInt32, UInt32) -> Void)?

    private let connection: NWConnection
    private let decoder: H264Decoder

    /// All reads/writes of `readBuffer` happen on `sendQueue` (the connection's
    /// receive queue), so it is accessed serially.
    private var readBuffer = Data()
    private let sendQueue = DispatchQueue(label: "com.touchmonitor.send")

    init?(host: String, port: UInt16, decoder: H264Decoder) {
        guard let nwPort = NWEndpoint.Port(rawValue: port) else { return nil }
        let endpoint = NWEndpoint.hostPort(host: NWEndpoint.Host(host), port: nwPort)
        self.connection = NWConnection(to: endpoint, using: .tcp)
        self.decoder = decoder
    }

    func start() {
        state = .connecting
        connection.stateUpdateHandler = { [weak self] newState in
            guard let self = self else { return }
            switch newState {
            case .ready:
                self.state = .connected
                self.receiveLoop()
            case .failed(let error):
                self.state = .failed(error.localizedDescription)
            case .cancelled:
                self.state = .disconnected
            case .waiting(let error):
                self.onStatus?("Waiting... \(error.localizedDescription)")
            default:
                break
            }
        }
        connection.start(queue: sendQueue)
    }

    func stop() {
        connection.cancel()
        state = .disconnected
    }

    private func receiveLoop() {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) { [weak self] data, _, isComplete, error in
            guard let self = self else { return }
            if let data = data {
                self.readBuffer.append(data)
                self.drainMessages()
            }
            if isComplete {
                self.state = .disconnected
                return
            }
            if let error = error {
                self.state = .failed(error.localizedDescription)
                return
            }
            self.receiveLoop()
        }
    }

    private func drainMessages() {
        while true {
            let result: (StreamMessageType, Data)?
            do {
                result = try StreamProtocol.popMessage(from: &readBuffer)
            } catch {
                onStatus?("Protocol error: \(error.localizedDescription)")
                stop()
                return
            }
            guard let (type, payload) = result else { break }
            handle(type, payload)
        }
    }

    private func handle(_ type: StreamMessageType, _ payload: Data) {
        switch type {
        case .video:
            if let parsed = StreamProtocol.parseVideoPayload(payload) {
                onVideoMeta?(parsed.width, parsed.height)
                decoder.decode(accessUnit: parsed.h264)
            }
        case .info:
            if let text = String(data: payload, encoding: .utf8) {
                onStatus?(text)
            }
        case .hello, .touch, .ping:
            break // not expected from the server; ignore
        }
    }

    /// Serializes and sends a batch of touches.
    /// `events` = [(id, active, x01, y01)] with normalized 0..1 coordinates.
    func sendTouches(_ events: [(id: UInt8, active: Bool, x: Float, y: Float)]) {
        NSLog("[TouchMonitor] sendTouches events=\(events.count) state=\(stateDesc)")
        if case .connected = state, !events.isEmpty {
            let message = StreamProtocol.makeTouchMessage(events: events)
            connection.send(content: message, completion: .contentProcessed { _ in })
            NSLog("[TouchMonitor] sendTouches -> sent \(message.count) bytes")
        } else {
            NSLog("[TouchMonitor] sendTouches -> SKIPPED (not connected or empty)")
        }
    }

    private var stateDesc: String {
        switch state {
        case .idle: return "idle"
        case .connecting: return "connecting"
        case .connected: return "connected"
        case .failed(let e): return "failed(\(e))"
        case .disconnected: return "disconnected"
        }
    }

    /// Sends a keep-alive ping.
    func sendPing() {
        if case .connected = state {
            let ping = StreamProtocol.frame(type: .ping, payload: Data())
            connection.send(content: ping, completion: .contentProcessed { _ in })
        }
    }
}
