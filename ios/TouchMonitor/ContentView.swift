import SwiftUI
import UIKit

struct ContentView: View {
    @StateObject private var model = AppModel()

    @State private var host: String = "192.168.42.1"
    @State private var port: String = "5666"

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
        .onAppear {
            host = model.host
            port = String(model.port)
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

            Text("Connect to the PC server over the USB link.")
                .font(.subheadline)
                .foregroundColor(.secondary)

            VStack(spacing: 12) {
                TextField("Host", text: $host)
                    .textContentType(.URL)
                    .keyboardType(.numbersAndPunctuation)
                    .autocapitalization(.none)
                    .disableAutocorrection(true)
                    .padding(12)
                    .background(Color(.secondarySystemBackground))
                    .cornerRadius(10)

                TextField("Port", text: $port)
                    .keyboardType(.numberPad)
                    .padding(12)
                    .background(Color(.secondarySystemBackground))
                    .cornerRadius(10)
            }
            .padding(.horizontal, 40)

            if !model.statusText.isEmpty {
                Text(model.statusText)
                    .font(.footnote)
                    .foregroundColor(.secondary)
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 32)
            }

            Button {
                model.host = host
                model.port = UInt16(port) ?? 5666
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

            Spacer()
        }
        .padding()
    }
}

/// Displays the decoded desktop and forwards multitouch to the server.
private struct StreamView: UIViewRepresentable {
    let model: AppModel

    func makeUIView(context: Context) -> StreamSurfaceView {
        let view = StreamSurfaceView(frame: .zero)
        context.coordinator.attach(model: model, to: view)
        return view
    }

    func updateUIView(_ uiView: StreamSurfaceView, context: Context) {
        // video + touch wiring is done once in the coordinator.
    }

    func makeCoordinator() -> Coordinator {
        Coordinator()
    }

    final class Coordinator {
        func attach(model: AppModel, to view: StreamSurfaceView) {
            model.onFrame = { [weak view] pixel in
                view?.display(pixelBuffer: pixel)
            }
            view.onTouchEvents = { [weak model] events in
                model?.sendTouches(events)
            }
        }
    }
}
