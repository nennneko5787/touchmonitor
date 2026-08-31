import Foundation
import Network

/// Owns the TCP connection to the PC server and pumps protocol messages.
///
/// * A receive loop accumulates bytes and pops framed messages (`StreamProtocol`).
/// * TCP carries touch/control traffic; video arrives as best-effort UDP.
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

    private var connection: NWConnection?
    private var videoConnection: NWConnection?
    private var serverEndpoint: NWEndpoint?
    private var browser: NWBrowser?
    private var usbListener: NWListener?
    private let decoder: H264Decoder

    /// Periodic keep-alive so the PC server can see the client->server path is
    /// alive even when the user isn't touching (helps diagnose touch drops).
    private var pingTimer: Timer?

    /// All reads/writes of `readBuffer` happen on `sendQueue` (the connection's
    /// receive queue), so it is accessed serially.
    private var readBuffer = Data()
    private var videoParts: [UInt32: (total: UInt16, keyframe: Bool, width: UInt32, height: UInt32, parts: [UInt16: Data])] = [:]
    private var lastDeliveredVideoFrame: UInt32?
    private let sendQueue = DispatchQueue(label: "com.touchmonitor.send")

    init(decoder: H264Decoder) {
        self.decoder = decoder
    }

    func start() {
        state = .connecting
        // Permit Bonjour discovery over Wi-Fi and peer-to-peer interfaces.
        // The latter is useful for Apple-managed local links (including USB
        // networking when the OS exposes it as a peer interface).
        let parameters = NWParameters.tcp
        parameters.includePeerToPeer = true
        let browser = NWBrowser(for: .bonjour(type: "_touchmonitor._tcp", domain: nil), using: parameters)
        self.browser = browser
        browser.stateUpdateHandler = { [weak self] state in
            guard let self = self else { return }
            switch state {
            case .ready:
                self.onStatus?("Bonjour ready (_touchmonitor._tcp)")
            case .failed(let error):
                self.onStatus?("Bonjour failed: \(error.localizedDescription)")
                self.state = .failed(error.localizedDescription)
            case .waiting(let error):
                self.onStatus?("Bonjour waiting: \(error.localizedDescription)")
            case .cancelled:
                break
            case .setup:
                self.onStatus?("Bonjour setup")
            @unknown default:
                break
            }
        }
        browser.browseResultsChangedHandler = { [weak self] results, _ in
            guard let self = self else { return }
            self.onStatus?("Bonjour services found: \(results.count)")
            guard self.connection == nil, let endpoint = results.first?.endpoint else { return }
            self.onStatus?("Opening TCP endpoint: \(endpoint.debugDescription)")
            self.open(endpoint: endpoint)
        }
        browser.start(queue: sendQueue)
    }

    /// Starts the USB transport. The iOS app listens only on loopback; the PC
    /// side exposes that socket through usbmuxd/iproxy. This avoids Bonjour and
    /// does not require the PC's network address to be known by the app.
    func startUSB() {
        stop()
        state = .connecting

        do {
            let parameters = NWParameters.tcp
            parameters.requiredInterfaceType = .loopback
            guard let port = NWEndpoint.Port(rawValue: 5666) else {
                throw NSError(domain: "TouchMonitor", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid USB listener port"])
            }
            let listener = try NWListener(using: parameters, on: port)
            usbListener = listener
            listener.stateUpdateHandler = { [weak self] state in
                switch state {
                case .ready:
                    self?.onStatus?("USB listener ready (127.0.0.1:5666)")
                case .failed(let error):
                    self?.onStatus?("USB listener failed: \(error.localizedDescription)")
                    self?.state = .failed(error.localizedDescription)
                case .waiting(let error):
                    self?.onStatus?("USB listener waiting: \(error.localizedDescription)")
                case .cancelled:
                    break
                default:
                    break
                }
            }
            listener.newConnectionHandler = { [weak self] connection in
                guard let self = self else { return }
                if self.connection != nil {
                    connection.cancel()
                    return
                }
                self.onStatus?("USB connection accepted")
                self.attach(connection: connection, endpointDescription: "USB/usbmuxd")
            }
            listener.start(queue: sendQueue)
        } catch {
            onStatus?("USB listener setup failed: \(error.localizedDescription)")
            state = .failed(error.localizedDescription)
        }
    }

    private func open(endpoint: NWEndpoint) {
        serverEndpoint = endpoint
        let parameters = NWParameters.tcp
        parameters.includePeerToPeer = true
        let connection = NWConnection(to: endpoint, using: parameters)
        attach(connection: connection, endpointDescription: endpoint.debugDescription)
    }

    private func attach(connection: NWConnection, endpointDescription: String) {
        self.connection = connection
        connection.stateUpdateHandler = { [weak self] newState in
            guard let self = self else { return }
            switch newState {
            case .setup:
                self.onStatus?("TCP setup")
            case .preparing:
                self.onStatus?("TCP resolving service endpoint")
            case .ready:
                self.onStatus?("TCP ready (\(endpointDescription))")
                self.state = .connected
                self.startPingTimer()
                self.receiveLoop()
            case .failed(let error):
                self.onStatus?("TCP failed: \(error.localizedDescription)")
                self.state = .failed(error.localizedDescription)
            case .cancelled:
                self.onStatus?("TCP cancelled")
                self.state = .disconnected
            case .waiting(let error):
                self.onStatus?("TCP waiting: \(error.localizedDescription)")
            }
        }
        connection.start(queue: sendQueue)
    }

    func stop() {
        stopPingTimer()
        browser?.cancel()
        browser = nil
        usbListener?.cancel()
        usbListener = nil
        connection?.cancel()
        connection = nil
        videoConnection?.cancel()
        videoConnection = nil
        state = .disconnected
    }

    private func receiveLoop() {
        guard let connection = connection else { return }
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
        case .hello:
            guard let videoPort = StreamProtocol.parseHello(payload) else { return }
            // A new server connection starts its u32 frame sequence at zero.
            videoParts.removeAll()
            lastDeliveredVideoFrame = nil
            if videoPort == 0 {
                // USB/usbmuxd mode: the server deliberately advertises port 0
                // because video is carried as framed MSG_VIDEO TCP messages.
                onStatus?("USB hello received; video uses TCP")
            } else if let endpoint = serverEndpoint {
                let udp = NWConnection(to: endpoint, using: .udp)
                videoConnection = udp
                udp.stateUpdateHandler = { [weak self] state in
                    if case .ready = state { self?.receiveVideoLoop() }
                }
                udp.start(queue: sendQueue)
                udp.send(content: Data([0x54, 0x4D, 0x52, 0x45, 0x47, 0x31]), completion: .contentProcessed { _ in })
            }
        case .touch, .ping:
            break
        }
    }

    private func receiveVideoLoop() {
        guard let udp = videoConnection else { return }
        udp.receiveMessage { [weak self] data, _, _, error in
            guard let self = self else { return }
            if let data = data { self.handleVideoDatagram(data) }
            if error == nil { self.receiveVideoLoop() }
        }
    }

    private func handleVideoDatagram(_ data: Data) {
        guard let packet = StreamProtocol.parseVideoDatagram(data) else { return }
        // A delayed P-frame must never be decoded after a newer frame: doing
        // so replaces the displayed reference image with stale, corrupted
        // content and makes only moving areas appear to update.
        if let last = lastDeliveredVideoFrame, !isNewerVideoFrame(packet.frame, than: last) {
            videoParts.removeValue(forKey: packet.frame)
            return
        }
        var entry = videoParts[packet.frame] ?? (packet.total, packet.keyframe, packet.width, packet.height, [:])
        guard entry.total == packet.total else { return }
        entry.parts[packet.index] = packet.bytes
        videoParts[packet.frame] = entry
        guard entry.parts.count == Int(entry.total) else {
            let oldest = packet.frame > 2 ? packet.frame - 2 : 0
            videoParts = videoParts.filter { $0.key >= oldest }
            return
        }
        var accessUnit = Data()
        for index in 0..<entry.total { guard let part = entry.parts[index] else { return }; accessUnit.append(part) }
        videoParts.removeValue(forKey: packet.frame)
        if let last = lastDeliveredVideoFrame, !isNewerVideoFrame(packet.frame, than: last) {
            return
        }
        lastDeliveredVideoFrame = packet.frame
        onVideoMeta?(entry.width, entry.height)
        decoder.decode(accessUnit: accessUnit)
    }

    private func isNewerVideoFrame(_ candidate: UInt32, than previous: UInt32) -> Bool {
        // Sequence numbers wrap; signed subtraction preserves ordering until
        // the receiver is more than 2^31 frames behind.
        return Int32(bitPattern: candidate &- previous) > 0
    }

    /// Serializes and sends a batch of touches.
    /// `events` = [(id, active, x01, y01)] with normalized 0..1 coordinates.
    func sendTouches(_ events: [(id: UInt8, active: Bool, x: Float, y: Float)]) {
        NSLog("[TouchMonitor] sendTouches events=\(events.count) state=\(stateDesc)")
        if case .connected = state, !events.isEmpty {
            let message = StreamProtocol.makeTouchMessage(events: events)
            connection?.send(content: message, completion: .contentProcessed { _ in })
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
        guard case .connected = state else { return }
        let ping = StreamProtocol.frame(type: .ping, payload: Data())
        connection?.send(content: ping, completion: .contentProcessed { _ in })
    }

    private func startPingTimer() {
        stopPingTimer()
        let timer = Timer(timeInterval: 2.0, repeats: true) { [weak self] _ in
            self?.sendPing()
        }
        RunLoop.main.add(timer, forMode: .common)
        pingTimer = timer
    }

    private func stopPingTimer() {
        pingTimer?.invalidate()
        pingTimer = nil
    }
}
