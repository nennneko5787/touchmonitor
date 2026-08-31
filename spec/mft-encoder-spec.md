# MFT Encoder Specification

## Overview
This document specifies the implementation of a GPU-accelerated H.264 encoder using Windows Media Foundation (MFT) to replace the current OpenH264 software encoder in the touchmonitor PC server.

## Goals
1. Replace software H.264 encoding with hardware-accelerated encoding via MFT
2. Maintain compatibility with existing iOS client (VideoToolbox decoder)
3. Output Annex-B H.264 byte stream format
4. Ensure low-latency encoding suitable for real-time desktop streaming
5. Support keyframe intervals configurable for immediate decodability on connect

## Requirements

### Input
- Format: BGRA8 (from Windows Graphics Capture)
- Resolution: Variable (captured monitor dimensions)
- Framerate: Configurable (typically 60 FPS)

### Output
- Format: Annex-B H.264 byte stream
- Profile: Baseline
- Encoding: Hardware-accelerated via MFT (Intel QSV, AMD VCN, NVIDIA NVENC)
- Bitrate: Configurable (default 8000 kbps)
- Keyframe interval: Configurable (initially set to 1 for immediate IDR frames)

### Functional Requirements
1. Initialize Media Foundation runtime
2. Enumerate and select appropriate H.264 encoder MFT (prefer hardware)
3. Configure input type as NV12 (requires BGRA→NV12 conversion)
4. Configure output type as H.264 Annex-B
5. Handle asynchronous MFT operation via IMFMediaEventGenerator
6. Process frames: wait for input event → convert BGRA→NV12 → process input → drain output
7. Detect keyframes via MFSampleExtension_CleanPoint
8. Output Annex-B formatted NAL units with proper start codes
9. Handle flush and end-of-stream messages properly
10. Release all resources on cleanup

### Non-Functional Requirements
- Latency: Target < 33ms per frame at 60 FPS
- CPU usage: Significantly lower than OpenH264 software encoder
- Reliability: No memory leaks, proper error handling
- Compatibility: Output must be decodable by iOS VideoToolbox

## Implementation Details

### Architecture
```
+------------------+     BGRA8 Frames     +---------------------+
| Windows Graphics | -------------------> | BGRA→NV12 Conversion |
|   Capture        |                      +---------------------+
+------------------+                                  |
                                                     v
+------------------+     NV12 Frames      +------------------+
|   MFT Encoder    | -------------------> |   H.264 NAL Units  |
| (IMFTransform)   | <------------------- | (Annex-B Byte Stream)|
+------------------+     Events           +------------------+
       ^                                                  |
       |                                                  v
+------------------+     Control      +------------------+
|  Event Loop      | <----------------|   Main Thread      |
| (METransformNeedInput/METransformHaveOutput) |                  |
+------------------+                  +------------------+
```

### Key Components

#### 1. MFTEncoder Struct
```rust
pub struct MftEncoder {
    transform: Option<IMFTransform>,           // The MFT transform
    event_gen: Option<IMFMediaEventGenerator>, // For async event handling
    input_stream_id: u32,                      // Input stream ID (always 0)
    output_stream_id: u32,                     // Output stream ID (always 0)
    width: u32,                                // Frame width
    height: u32,                               // Frame height
    output_buf_size: u32,                      // Output buffer size estimate
}
```

#### 2. EncodedFrame Struct
```rust
pub struct EncodedFrame {
    pub data: Vec<u8>,   // Annex-B H.264 byte stream
    pub keyframe: bool,  // Whether frame is IDR
}
```

#### 3. Initialization Process
1. Initialize Media Foundation (`MFStartup`)
2. Enumerate hardware H.264 encoder MFTs
   - Fallback to software MFT if none found
3. Activate the selected MFT
4. Query `IMFMediaEventGenerator` interface
5. Set `MF_TRANSFORM_ASYNC_UNLOCK` attribute to 1
6. Configure output media type:
   - Major type: `MFMediaType_Video`
   - Subtype: `MFVideoFormat_H264`
   - Average bitrate: `bitrate_kbps * 1000`
   - Frame size: `{width, height}`
   - Frame rate: `{fps, 1}`
   - Pixel aspect ratio: `{1, 1}`
   - Interlace mode: Progressive
   - MPEG2 profile: Base (`eAVEncH264VProfile_Base`)
   - Max keyframe spacing: Configurable (initially 1)
7. Configure input media type:
   - Major type: `MFMediaType_Video`
   - Subtype: `MFVideoFormat_NV12`
   - Frame size: `{width, height}`
   - Frame rate: `{fps, 1}`
   - Pixel aspect ratio: `{1, 1}`
   - Interlace mode: Progressive
   - Default stride: `width`
8. Send initialization messages:
   - `MFT_MESSAGE_NOTIFY_BEGIN_STREAMING`
   - `MFT_MESSAGE_NOTIFY_START_OF_STREAM`

#### 4. Frame Encoding Process (`encode_frame`)
For each input BGRA frame:

1. **Wait for input event**: Block until `METransformNeedInput` event
2. **Convert BGRA to NV12**:
   - Luma (Y): `0.257*R + 0.504*G + 0.098*B + 16.0`
   - Chroma U: `-0.148*R - 0.291*G + 0.439*B + 128.0`
   - Chroma V: `0.439*R - 0.368*G - 0.071*B + 128.0`
   - Clamp to [0, 255], convert to uint8
   - UV plane sampled at 2x2 intervals (width × height/2)
3. **Create input sample**:
   - Create `IMFSample`
   - Add Y plane buffer
   - Add UV plane buffer
4. **Process input**: Call `ProcessInput` on the transform
5. **Drain output**:
   - Poll for `METransformHaveOutput` events (non-blocking)
   - For each event:
     - Create output sample with buffer
     - Call `ProcessOutput`
     - Extract encoded data from sample
     - Check `MFSampleExtension_CleanPoint` for keyframe flag
   - If no output after polling:
     - Block until `METransformHaveOutput` event
     - Process that output
6. Return encoded data and keyframe flag

#### 5. Error Handling
- All Windows API calls return `windows::core::Result`
- Convert to `Box<dyn std::error::Error>` for public interface
- Specific error cases:
  - MF initialization failure
  - No H.264 encoder MFT found
  - Activation or configuration failures
  - Timeout waiting for events
  - Empty output from encoder

#### 6. Cleanup (`Drop` impl)
When encoder is dropped:
1. Send flush command: `MFT_MESSAGE_COMMAND_FLUSH`
2. Send end-of-stream: `MFT_MESSAGE_NOTIFY_END_OF_STREAM`
3. Send end-streaming: `MFT_MESSAGE_NOTIFY_END_STREAMING`
4. Resources automatically released via smart pointers

## Integration with PC Server

### Changes to stream.rs
- Replace `Encoder::OpenH264(...)` with `Encoder::Mft(MftEncoder::new(...))`
- Remove OpenH264-specific imports if no longer needed elsewhere
- Ensure proper error propagation from MftEncoder

### Threading Considerations
- MFT encoder is not thread-safe; use from single thread (writer loop)
- Current architecture already isolates encoding to writer thread
- No additional synchronization needed

## Configuration Parameters
- `width`: Monitor width in pixels
- `height`: Monitor height in pixels
- `fps`: Frames per second (typically 60)
- `bitrate_kbps`: Target bitrate in kilobits per second (default 8000)
- `keyframe_interval`: Maximum frames between keyframes (default 1 for testing, should be configurable to fps for normal operation)

## Performance Expectations
- CPU usage: 50-80% reduction compared to OpenH264
- Latency: Consistent < 33ms per frame
- Throughput: Able to handle 60 FPS at 1080p+ resolutions
- Power efficiency: Significantly better on laptops/tablets

## Testing Strategy
1. Unit tests:
   - BGRA→NV12 conversion accuracy
   - Media type configuration
   - Error handling paths

2. Integration tests:
   - Verify encoder initializes with hardware MFT when available
   - Verify output is valid Annex-B H.264
   - Verify keyframes are produced at configured intervals
   - Verify iOS client can decode stream without black screen
   - Verify touch functionality remains intact

3. Performance tests:
   - Measure CPU usage vs OpenH264
   - Measure end-to-end latency
   - Test at various resolutions and bitrates

## Future Enhancements
- Dynamic bitrate adjustment based on network conditions
- Scene change detection for adaptive keyframe placement
- Support for VC-1 or other codecs if needed
- Multi-stream encoding for multiple clients
- Rate control parameters tuning (VBR/CBR, quality vs bitrate)

## Open Questions
1. What is the optimal keyframe interval for production? (Initially 1 for testing, should be configurable)
2. Should we support configurable encoding parameters (quality vs speed tradeoffs)?
3. How to handle encoder device loss/recovery scenarios?
4. Should we implement fallback to software encoder if hardware encoder fails?

## References
- Windows Media Foundation documentation
- MSDN: Media Foundation Transforms
- H.264/AVC standard (ISO/IEC 14496-10)
- Annex-B byte stream format specification
- VideoToolbox decoding requirements on iOS