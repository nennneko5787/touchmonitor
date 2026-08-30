import UIKit
import CoreVideo
import CoreImage

/// A single UIKit view that both:
///   1. displays the most recent decoded video frame, and
///   2. captures real multitouch and reports it as normalized (0..1) desktop coords.
///
/// Reported touch events follow the wire format `(id, active, x01, y01)`.
final class StreamSurfaceView: UIView {

    /// Called with a batch of touch events whenever fingers move / land / lift.
    /// Coordinates are normalized (0..1) relative to the aspect-fit desktop rect.
    var onTouchEvents: (([(id: UInt8, active: Bool, x: Float, y: Float)]) -> Void)?

    /// Size of the desktop in pixels, updated as frames arrive. Used for aspect-fit.
    var videoSize: CGSize = .zero

    private let renderQueue = DispatchQueue(label: "com.touchmonitor.render")
    private let ciContext = CIContext(options: [.cacheIntermediates: false])

    // Per-finger bookkeeping: pointer identity -> (assigned id, normalized point).
    private var touchMap: [ObjectIdentifier: (id: UInt8, point: CGPoint)] = [:]
    private var nextTouchID: UInt8 = 0
    private var touchLock = NSLock()

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
        isUserInteractionEnabled = true
        backgroundColor = .black
        layer.contentsGravity = .resizeAspect
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    // MARK: - Video rendering

    func display(pixelBuffer: CVPixelBuffer) {
        // Capturing `pixelBuffer` in the async block keeps it alive (ARC manages
        // CVPixelBuffer automatically) until the render work is done.
        renderQueue.async { [weak self] in
            guard let self = self else { return }
            let ciImage = CIImage(cvPixelBuffer: pixelBuffer)
            guard let cgImage = self.ciContext.createCGImage(ciImage, from: ciImage.extent) else { return }
            DispatchQueue.main.async {
                self.layer.contents = cgImage
            }
        }
    }

    // MARK: - Multitouch capture

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        for touch in touches {
            let id = nextTouchID
            nextTouchID = nextTouchID &+ 1
            let point = normalized(touch, in: bounds)
            touchLock.lock()
            touchMap[ObjectIdentifier(touch)] = (id, point)
            touchLock.unlock()
            emitBatch()
        }
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        var changed = false
        for touch in touches {
            let point = normalized(touch, in: bounds)
            touchLock.lock()
            if var stored = touchMap[ObjectIdentifier(touch)] {
                stored.point = point
                touchMap[ObjectIdentifier(touch)] = stored
                changed = true
            }
            touchLock.unlock()
        }
        if changed { emitBatch() }
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        for touch in touches {
            let point = normalized(touch, in: bounds)
            touchLock.lock()
            if let stored = touchMap.removeValue(forKey: ObjectIdentifier(touch)) {
                touchLock.unlock()
                emitInactive(id: stored.id, point: point)
            } else {
                touchLock.unlock()
            }
        }
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        touchesEnded(touches, with: event)
    }

    private func emitBatch() {
        touchLock.lock()
        let snapshot = touchMap.map { $0.value }
        touchLock.unlock()
        let events = snapshot.compactMap { entry -> (id: UInt8, active: Bool, x: Float, y: Float)? in
            (entry.id, true, Float(entry.point.x), Float(entry.point.y))
        }
        onTouchEvents?(events)
    }

    private func emitInactive(id: UInt8, point: CGPoint) {
        onTouchEvents?([(id, false, Float(point.x), Float(point.y))])
    }

    /// Maps a UIKit point to normalized (0..1) coordinates within the aspect-fit
    /// desktop rect. Returns (0,0) if the touch is outside the desktop area.
    private func normalized(_ touch: UITouch, in bounds: CGRect) -> CGPoint {
        let loc = touch.location(in: self)
        return normalizedPoint(loc, bounds: bounds, video: videoSize)
    }

    private func normalizedPoint(_ point: CGPoint, bounds: CGRect, video: CGSize) -> CGPoint {
        guard video.width > 0, video.height > 0 else {
            return CGPoint(x: Double(point.x) / Double(bounds.width),
                           y: Double(point.y) / Double(bounds.height))
        }
        let scale = min(bounds.width / video.width, bounds.height / video.height)
        let dw = video.width * scale
        let dh = video.height * scale
        let ox = (bounds.width - dw) / 2
        let oy = (bounds.height - dh) / 2
        let desktopRect = CGRect(x: ox, y: oy, width: dw, height: dh)
        guard desktopRect.contains(point) else { return .zero }
        return CGPoint(x: (point.x - ox) / dw, y: (point.y - oy) / dh)
    }
}
