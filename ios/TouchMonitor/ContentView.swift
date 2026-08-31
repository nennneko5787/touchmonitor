import SwiftUI
import UIKit
import Combine

struct ContentView: View {
    @StateObject private var model = AppModel()
    @State private var showingLogs = false

    var body: some View {
        Group {
            if model.isConnected {
                StreamView(model: model)
                    .allowsHitTesting(true)
                    .edgesIgnoringSafeArea(.all)
            } else {
                connectScreen
            }
        }
        .sheet(isPresented: $showingLogs) {
            LogView(model: model)
        }
        .onAppear {
        }
        .onDisappear {
            model.disconnect()
        }
    }

    private var connectScreen: some View {
        VStack(spacing: 20) {
            Spacer()
            Image(systemName: "rectangle.connected.to.line.below")
                .font(.system(size: 56))
                .foregroundColor(.accentColor)

            Text("TouchMonitor")
                .font(.largeTitle.bold())

            Text("Build \(model.buildVersion)")
                .font(.caption)
                .foregroundColor(.secondary)

            Text("Connect to the PC server over the USB link.")
                .font(.subheadline)
                .foregroundColor(.secondary)

            if !model.statusText.isEmpty {
                Text(model.statusText)
                    .font(.footnote)
                    .foregroundColor(.secondary)
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 32)
            }

            Button {
                model.connect()
            } label: {
                Text("Connect")
                    .fontWeight(.semibold)
                    .frame(maxWidth: .infinity)
                    .padding()
                    .background(Color.accentColor)
                    .foregroundColor(.white)
                    .cornerRadius(12)
            }
            .padding(.horizontal, 40)

            Button("View connection log") {
                showingLogs = true
            }
            .padding(.top, 4)

            Spacer()
        }
        .padding()
    }
}

private struct LogView: View {
    @ObservedObject var model: AppModel
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationView {
            Group {
                if model.logs.isEmpty {
                    VStack(spacing: 12) {
                        Image(systemName: "doc.text")
                            .font(.largeTitle)
                        Text("No connection log")
                            .foregroundColor(.secondary)
                    }
                } else {
                    ScrollViewReader { proxy in
                        List(Array(model.logs.enumerated()), id: \.offset) { index, line in
                            Text(line)
                                .font(.system(.caption, design: .monospaced))
                                .textSelection(.enabled)
                                .id(index)
                        }
                        .onAppear {
                            if let last = model.logs.indices.last { proxy.scrollTo(last) }
                        }
                    }
                }
            }
            .navigationTitle("Connection Log")
            .toolbar {
                ToolbarItem(placement: .navigationBarLeading) {
                    Button("Clear") { model.clearLogs() }
                }
                ToolbarItem(placement: .navigationBarLeading) {
                    Button("Copy") { UIPasteboard.general.string = model.copyableLogs() }
                }
                ToolbarItem(placement: .navigationBarTrailing) {
                    Button("Done") { dismiss() }
                }
            }
        }
    }
}

/// Displays the decoded desktop and forwards multitouch to the server.
///
/// Hosted by a UIViewController (rather than a bare UIView) so that the touch
/// stream reaches the surface reliably inside SwiftUI.
private struct StreamView: UIViewControllerRepresentable {
    let model: AppModel

    func makeUIViewController(context: Context) -> StreamHostViewController {
        let controller = StreamHostViewController()
        let surface = StreamSurfaceView(frame: .zero)
        controller.setSurface(surface)
        context.coordinator.attach(model: model, to: surface)
        return controller
    }

    func updateUIViewController(_ uiViewController: StreamHostViewController, context: Context) {
        // video + touch wiring is done once in the coordinator.
    }

    func makeCoordinator() -> Coordinator {
        Coordinator()
    }

    final class Coordinator {
        private var cancellable: AnyCancellable?

        func attach(model: AppModel, to view: StreamSurfaceView) {
            model.onFrame = { [weak view] pixel in
                view?.display(pixelBuffer: pixel)
            }
            view.onTouchEvents = { [weak model] events in
                model?.sendTouches(events)
            }

            // Update view.videoSize when model.videoSize changes.
            cancellable = model.$videoSize
                .sink { [weak view] size in
                    view?.videoSize = size
                }
        }

        deinit {
            cancellable?.cancel()
        }
    }
}

/// A minimal UIViewController whose `view` is the `StreamSurfaceView`.
///
/// Hosting the surface under a UIViewController guarantees its view is
/// treated as a first-class, hit-testable surface by the system, so touches
/// reach `touchesBegan/Ended` even when presented through SwiftUI.
final class StreamHostViewController: UIViewController {
    private(set) var surface: StreamSurfaceView?

    func setSurface(_ view: StreamSurfaceView) {
        surface = view
    }

    override func loadView() {
        if let surface = surface {
            view = surface
        } else {
            view = UIView()
        }
    }
}
