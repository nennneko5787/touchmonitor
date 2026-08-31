import Foundation
import CoreVideo
import CoreGraphics
import Combine

/// Application-level state: owns the H.264 decoder and the network client, and
/// exposes connection state for the SwiftUI layer.
final class AppModel: ObservableObject {

    var buildVersion: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "unknown"
    }

    @Published var state: NetworkClient.State = .idle
    @Published var statusText: String = ""
    @Published var videoSize: CGSize = .zero
    @Published private(set) var logs: [String] = []

    /// Set by the stream view so decoded frames can be rendered.
    var onFrame: ((CVPixelBuffer) -> Void)?

    let decoder = H264Decoder()
    private var client: NetworkClient?

    init() {
        decoder.onDecodedFrame = { [weak self] pixel in
            guard let self = self else { return }
            self.onFrame?(pixel)
        }
    }

    var isConnected: Bool {
        if case .connected = state { return true }
        return false
    }

    func connect() {
        disconnectInternal()
        logs.removeAll()
        appendLog("Connect requested; build \(buildVersion)")
        let candidate = NetworkClient(decoder: decoder)
        candidate.onStateChange = { [weak self] newState in
            DispatchQueue.main.async {
                self?.state = newState
                self?.syncStateText(newState)
                self?.appendLog("State: \(self?.stateDescription(newState) ?? "unknown")")
            }
        }
        candidate.onStatus = { [weak self] text in
            DispatchQueue.main.async {
                self?.statusText = text
                self?.appendLog(text)
            }
        }
        candidate.onVideoMeta = { [weak self] w, h in
            DispatchQueue.main.async {
                self?.videoSize = CGSize(width: CGFloat(w), height: CGFloat(h))
            }
        }
        client = candidate
        candidate.start()
    }

    func connectUSB() {
        disconnectInternal()
        logs.removeAll()
        appendLog("USB connect requested; build \(buildVersion)")
        let candidate = NetworkClient(decoder: decoder)
        candidate.onStateChange = { [weak self] newState in
            DispatchQueue.main.async {
                self?.state = newState
                self?.syncStateText(newState)
                self?.appendLog("State: \(self?.stateDescription(newState) ?? "unknown")")
            }
        }
        candidate.onStatus = { [weak self] text in
            DispatchQueue.main.async {
                self?.statusText = text
                self?.appendLog(text)
            }
        }
        candidate.onVideoMeta = { [weak self] w, h in
            DispatchQueue.main.async {
                self?.videoSize = CGSize(width: CGFloat(w), height: CGFloat(h))
            }
        }
        client = candidate
        candidate.startUSB()
    }

    func disconnect() {
        disconnectInternal()
        state = .disconnected
    }

    private func disconnectInternal() {
        client?.stop()
        client = nil
    }

    func sendTouches(_ events: [(id: UInt8, active: Bool, x: Float, y: Float)]) {
        client?.sendTouches(events)
    }

    func clearLogs() {
        logs.removeAll()
    }

    func copyableLogs() -> String {
        logs.joined(separator: "\n")
    }

    private func appendLog(_ message: String) {
        let formatter = DateFormatter()
        formatter.dateFormat = "HH:mm:ss.SSS"
        logs.append("[\(formatter.string(from: Date()))] \(message)")
        if logs.count > 300 {
            logs.removeFirst(logs.count - 300)
        }
    }

    private func stateDescription(_ state: NetworkClient.State) -> String {
        switch state {
        case .idle: return "idle"
        case .connecting: return "connecting"
        case .connected: return "connected"
        case .failed(let reason): return "failed: \(reason)"
        case .disconnected: return "disconnected"
        }
    }

    private func syncStateText(_ newState: NetworkClient.State) {
        switch newState {
        case .connected:
            statusText = "Connected over USB"
        case .failed(let reason):
            statusText = "Failed: \(reason)"
        case .disconnected:
            statusText = "Disconnected"
        case .idle, .connecting:
            break
        }
    }
}
