# Touch Coordinate System Specification

## Overview
This document specifies the touch coordinate mapping system used to translate touch input from the iOS client to desktop coordinates on the PC server. The system must handle different screen resolutions, aspect ratios, and display scaling between the iOS device and the PC monitor.

## Problem Statement
Users experienced touch coordinate drift, particularly in the Y-axis, where touches became increasingly inaccurate further from the center of the screen. This was caused by improper handling of:
1. Different resolutions between iOS device and PC monitor
2. Different aspect ratios leading to letterboxing/pillarboxing
3. Display scaling (DPI settings) on Windows
4. UIKit's coordinate system vs Windows coordinate system

## Goals
1. Accurately map iOS touch coordinates to desktop coordinates
2. Handle arbitrary desktop and iOS screen resolutions
3. Account for aspect-fit scaling used in video display
4. Compensate for display scaling on Windows (when applicable)
5. Provide normalized coordinates (0.0-1.0) for network transmission
6. Ensure touch accuracy across entire screen area, including edges and corners

## Definitions
- **Desktop Resolution**: The actual pixel resolution of the PC monitor being captured
- **iOS Screen Resolution**: The physical pixel resolution of the iOS device screen
- **Video Display Area**: The rectangle on iOS where the desktop video is displayed (aspect-fit within screen bounds)
- **Display Scale Factor**: The ratio between physical pixels and logical points (typically 1.0, 2.0, or 3.0 on iOS)
- **Normalized Coordinates**: Values in range [0.0, 1.0] where (0,0) is top-left and (1,1) is bottom-right

## Requirements

### Input
- Touch events from iOS client in normalized coordinates (0.0-1.0) relative to video display area
- Each touch event contains: (id: UInt8, active: Bool, x: Float, y: Float)
- Coordinates are already normalized to the video display area as displayed on iOS

### Output
- Touch coordinates in desktop pixel coordinates for Windows injection
- Must be accurate to within 1 pixel of intended target
- Must work across entire desktop surface

### Functional Requirements
1. Convert normalized touch coordinates (relative to video display area) to desktop pixel coordinates
2. Account for aspect-fit scaling used to fit desktop video on iOS screen
3. Handle cases where desktop and iOS screens have different aspect ratios
4. Compensate for iOS display scale factor (points vs pixels)
5. Clamp coordinates to valid desktop bounds
6. Support multi-touch tracking with persistent touch IDs
7. Provide both normalized (for network) and pixel (for injection) coordinate representations

### Non-Functional Requirements
- Computationally efficient (touch events should add <1ms latency)
- Deterministic mapping (same input always produces same output)
- Thread-safe for concurrent touch events
- No drift or accumulation of errors over time

## Implementation Details

### Coordinate Spaces
```
+------------------+     iOS Screen (Pixels)     +------------------+
|                  |                             |                  |
|  Video Display   |<--------------------------->|                  |
|   Area (Pixels)  |                             |   iOS Screen   |
|                  |  (Aspect-fit scaled)        |                  |
+------------------+                             +------------------+
        ^                                                  ^
        |                                                  |
        | Normalized Coordinates (0-1)                     | Display Scale
        |                                                  | (e.g., 2x for Retina)
        v                                                  v
+------------------+     Video Display Area     +------------------+
|                  |     (Points)               |                  |
|  Normalized      |<--------------------------->|   Desktop        |
|   Coordinates    |                            |   Coordinates    |
|   (Network)      |                            |   (Pixels)       |
+------------------+                            +------------------+
        ^                                                  ^
        |                                                  |
        | Inverse Aspect-fit Scaling                     | Windows Display
        |                                                  | Scale (if any)
        v                                                  v
+------------------+     Desktop Video Area     +------------------+
|                  |     (Pixels)               |                  |
|   Desktop        |<--------------------------->|   Physical       |
|   Coordinates    |                            |   Desktop        |
|   (Target)       |                            |   Pixels         |
+------------------+                            +------------------+
```

### Key Formulas

#### 1. iOS Normalized to Desktop Pixel Conversion
Given:
- `touch_point`: CGPoint from iOS in normalized coordinates (0-1) relative to video display area
- `video_size`: CGSize of desktop resolution in pixels (width, height)
- `bounds`: CGRect of iOS view in points
- `screen_scale`: UIScreen.main.scale (typically 2.0 or 3.0)

Steps:
1. Convert normalized coordinates to points within video display area:
   ```
   video_point_in_points = CGPoint(
       x: touch_point.x * video_size.width,
       y: touch_point.y * video_size.height
   )
   ```

2. Convert video display area points to iOS view points (accounting for aspect-fit):
   ```
   // Calculate aspect-fit scaling
   let video_aspect = video_size.width / video_size.height
   let view_aspect = bounds.width / bounds.height
   
   let scale: CGFloat
   let offset: CGPoint
   
   if video_aspect > view_aspect {
       // Video is wider than view - fit to height
       scale = bounds.height / video_size.height
       offset = CGPoint(
           x: (bounds.width - video_size.width * scale) / 2.0,
           y: 0
       )
   } else {
       // Video is taller than view - fit to width
       scale = bounds.width / video_size.width
       offset = CGPoint(
           x: 0,
           y: (bounds.height - video_size.height * scale) / 2.0
       )
   }
   
   // Convert video point to view point
   let view_point = CGPoint(
       x: video_point_in_points.x * scale + offset.x,
       y: video_point_in_points.y * scale + offset.y
   )
   ```

3. Convert view points to desktop pixel coordinates (inverse of above):
   ```
   let desktop_point = CGPoint(
       x: (view_point.x - offset.x) / scale,
       y: (view_point.y - offset.y) / scale
   )
   ```

4. Clamp to desktop bounds:
   ```
   let clamped_point = CGPoint(
       x: max(0, min(desktop_point.x, video_size.width - 1)),
       y: max(0, min(desktop_point.y, video_size.height - 1))
   )
   ```

### Data Flow

#### iOS Client Side
1. User touches screen → UIKit delivers touch event in view coordinates (points)
2. Convert to normalized coordinates relative to video display area:
   ```
   let normalized_x = (touch_point_in_view.x - video_origin_x) / video_width_in_points
   let normalized_y = (touch_point_in_view.y - video_origin_y) / video_height_in_points
   ```
3. Package as `(id, active, normalized_x, normalized_y)` and send to PC server
4. Display visual feedback (optional, removed in production to avoid occlusion)

#### PC Server Side
1. Receive normalized touch coordinates from network
2. Convert to desktop pixel coordinates using inverse of above process:
   - Use same aspect-fit calculation based on current video size
   - Apply inverse scaling to get desktop pixel coordinates
3. Inject touch via Windows API (`InjectSyntheticPointerInput`)
4. Maintain touch ID mapping for multi-touch persistence

### Implementation in StreamSurfaceView.swift

#### Properties
```swift
// Per-finger bookkeeping: pointer identity -> (assigned id, normalized point).
private var touchMap: [ObjectIdentifier: (id: UInt8, point: CGPoint)] = [:]
private var nextTouchID: UInt8 = 0
private var touchLock = NSLock()
private let screenScale = UIScreen.main.scale  // Cache for efficiency
```

#### Touch Event Handling
```swift
override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
    for touch in touches {
        let id = nextTouchID
        nextTouchID = nextTouchID &+ 1
        let loc = touch.location(in: self)
        let point = normalized(touch, in: bounds)
        touchLock.lock()
        touchMap[ObjectIdentifier(touch)] = (id, point)
        touchLock.unlock()
        emitBatch()
    }
}

// Similar for touchesMoved, touchesEnded, touchesCancelled
```

#### Coordinate Normalization (Key Method)
```swift
/// Maps a UIKit point to normalized (0..1) coordinates within the aspect-fit
/// desktop rect. Returns (0,0) if the touch is outside the desktop area.
private func normalized(_ touch: UITouch, in bounds: CGRect) -> CGPoint {
    let loc = touch.location(in: self)
    return normalizedPoint(loc, bounds: bounds, video: videoSize)
}

private func normalizedPoint(_ point: CGPoint, bounds: CGRect, video: CGSize) -> CGPoint {
    // Handle case where video size not yet known
    guard video.width > 0, video.height > 0 else {
        return CGPoint(x: Double(point.x) / Double(bounds.width),
                       y: Double(point.y) / Double(bounds.height))
    }
    
    // Convert video size from pixels to points using screen scale
    let videoSizeInPoints = CGSize(
        width: video.width / screenScale,
        height: video.height / screenScale
    )
    
    // Calculate aspect-fit scaling
    let videoAspect = videoSizeInPoints.width / videoSizeInPoints.height
    let viewAspect = bounds.width / bounds.height
    
    let scale: CGFloat
    let offset: CGPoint
    
    if videoAspect > viewAspect {
        // Video is wider than view - fit to height
        scale = bounds.height / videoSizeInPoints.height
        offset = CGPoint(
            x: (bounds.width - videoSizeInPoints.width * scale) / 2.0,
            y: 0
        )
    } else {
        // Video is taller than view - fit to width
        scale = bounds.width / videoSizeInPoints.width
        offset = CGPoint(
            x: 0,
            y: (bounds.height - videoSizeInPoints.height * scale) / 2.0
        )
    }
    
    let desktopRect = CGRect(
        x: offset.x,
        y: offset.y,
        width: videoSizeInPoints.width * scale,
        height: videoSizeInPoints.height * scale
    )
    
    // Check if point is within desktop area
    guard desktopRect.contains(point) else { return .zero }
    
    // Convert point to normalized coordinates within desktop rect
    return CGPoint(
        x: (point.x - offset.x) / (videoSizeInPoints.width * scale),
        y: (point.y - offset.y) / (videoSizeInPoints.height * scale)
    )
}
```

#### Emit Touch Events
```swift
private func emitBatch() {
    touchLock.lock()
    let snapshot = touchMap.map { $0.value }
    touchLock.unlock()
    
    let events = snapshot.compactMap { entry -> (id: UInt8, active: Bool, x: Float, y: Float)? in
        (entry.id, true, Float(entry.point.x), Float(entry.point.y))
    }
    
    NSLog("[TouchMonitor] emitBatch events=\(events.count)")
    onTouchEvents?(events)
}

private func emitInactive(id: UInt8, point: CGPoint) {
    onTouchEvents?([(id, false, Float(point.x), Float(point.y))])
}
```

## Integration with Touch Injector

### PC Server Side (TouchInjector)
The `TouchInjector` in `pc-server/src/touch.rs` receives the normalized coordinates and must convert them to desktop pixel coordinates for injection.

Given normalized coordinates (nx, ny) in range [0,1]:
1. Calculate desktop pixel coordinates:
   ```
   desktop_x = nx * desktop_width
   desktop_y = ny * desktop_height
   ```
2. Clamp to [0, desktop_width-1] and [0, desktop_height-1]
3. Inject via `InjectSyntheticPointerInput`

The injector already has the desktop bounds from `ScreenMapping`, so the conversion is straightforward.

## Testing Strategy

### Unit Tests
1. Test normalizedPoint function with various combinations:
   - Same aspect ratios (1:1, 16:9, 4:3)
   - Different aspect ratios (video wider than view, video taller than view)
   - Edge cases (touches at boundaries, corners)
   - Different screen scales (1.0, 2.0, 3.0)
   - Zero or invalid video sizes

### Integration Tests
1. End-to-end touch accuracy:
   - Use automated touch testing to verify touches land within 1 pixel of target
   - Test across entire screen area
   - Test with different iOS devices and PC monitor resolutions
   - Test multi-touch scenarios

### Manual Verification
1. Visual verification using on-screen markers (during development only)
2. Test drawing applications to verify line accuracy
3. Test UI interactions (buttons, sliders, etc.) across screen
4. Verify pinch-to-zoom and other multi-touch gestures work correctly

## Performance Considerations
- The coordinate transformation involves a few floating-point operations
- Should add negligible latency (<0.1ms) per touch event
- Minimal memory allocations (touches are transient)
- Thread-safe via locking on touchMap access

## Error Handling
- Invalid video sizes (zero dimensions) fall back to simple normalization
- Points outside video area are clamped to (0,0) indicating invalid touch
- All calculations use CGFloat (Double on 64-bit systems) for precision
- No error conditions should occur in normal operation

## Future Enhancements
1. Support for rotated display orientations
2. Compensation for Windows display scaling (if different from 100%)
3. Calibration system for per-user offset correction
4. Support for touch prediction to reduce perceived latency
5. Haptic feedback integration (if iOS device supports it)

## References
- Apple UIKit Touch Handling Guide
- Core Geometry Types (CGPoint, CGSize, CGRect)
- Aspect Ratio Scaling Algorithms
- Multi-touch Tracking Best Practices