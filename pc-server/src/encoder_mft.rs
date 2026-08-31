//! GPU-accelerated H.264 encoder using Windows Media Foundation (MFT).
//!
//! Hardware MFTs (Intel QSV, AMD VCN, NVIDIA NVENC) are async. We use
//! IMFMediaEventGenerator for METransformNeedInput / METransformHaveOutput
//! events.
//!
//! Input: BGRA8 frames (from Windows Graphics Capture).
//! Output: Annex-B H.264 access units.

use std::mem::ManuallyDrop;
use std::ptr;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use windows::core::{implement, Interface, Ref};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

const DEBUG: bool = false;

pub struct MftEncoder {
    transform: Option<IMFTransform>,
    event_gen: Option<IMFMediaEventGenerator>,
    input_stream_id: u32,
    output_stream_id: u32,
    width: u32,
    height: u32,
    fps: u32,
    output_buf_size: u32,
    output_provides_samples: bool,
    async_mode: bool,
    frame_index: i64,
    event_state: Arc<EventState>,
    event_callback: Option<IMFAsyncCallback>,
}

unsafe impl Send for MftEncoder {}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

fn mf_runtime_init() -> bool {
    static INIT: OnceLock<bool> = OnceLock::new();
    *INIT.get_or_init(|| unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        match MFStartup(MF_VERSION, MFSTARTUP_FULL) {
            Ok(()) => true,
            Err(_e) => {
                false
            }
        }
    })
}

/// COM initialization is per-thread. The encoder is created on the client
/// worker thread, not on the thread that accepts the socket.
pub fn ensure_com_initialized() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

fn enumerate_hw_h264_encoders() -> windows::core::Result<Vec<IMFActivate>> {
    unsafe {
        let output_type = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut activates: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count: u32 = 0;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG(
                MFT_ENUM_FLAG_HARDWARE.0
                    | MFT_ENUM_FLAG_ASYNCMFT.0
                    | MFT_ENUM_FLAG_SORTANDFILTER.0,
            ),
            None,
            Some(&output_type),
            &mut activates,
            &mut count,
        )?;
        let mut out = Vec::with_capacity(count as usize);
        if !activates.is_null() {
            for i in 0..count as usize {
                if let Some(activate) = (*activates.add(i)).take() {
                    out.push(activate);
                }
            }
            CoTaskMemFree(Some(activates as *const _));
        }
        Ok(out)
    }
}

fn enumerate_sw_h264_encoders() -> windows::core::Result<Vec<IMFActivate>> {
    unsafe {
        let output_type = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut activates: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count: u32 = 0;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG(MFT_ENUM_FLAG_SORTANDFILTER.0),
            None,
            Some(&output_type),
            &mut activates,
            &mut count,
        )?;
        let mut out = Vec::with_capacity(count as usize);
        if !activates.is_null() {
            for i in 0..count as usize {
                if let Some(activate) = (*activates.add(i)).take() {
                    out.push(activate);
                }
            }
            CoTaskMemFree(Some(activates as *const _));
        }
        Ok(out)
    }
}

fn pack_u32_pair(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | (b as u64)
}

fn bgra_to_nv12(bgra: &[u8], width: usize, height: usize) -> (Vec<u8>, Vec<u8>) {
    let mut y_plane = vec![0u8; width * height];
    let mut uv_plane = vec![0u8; width * (height / 2)];
    for row in 0..height {
        for col in 0..width {
            let idx = (row * width + col) * 4;
            let b = bgra[idx] as f64;
            let g = bgra[idx + 1] as f64;
            let r = bgra[idx + 2] as f64;
            y_plane[row * width + col] = (0.257 * r + 0.504 * g + 0.098 * b + 16.0)
                .clamp(0.0, 255.0) as u8;
        }
    }
    for row in (0..height).step_by(2) {
        for col in (0..width).step_by(2) {
            let idx = (row * width + col) * 4;
            let b = bgra[idx] as f64;
            let g = bgra[idx + 1] as f64;
            let r = bgra[idx + 2] as f64;
            uv_plane[(row / 2) * width + col] =
                (-0.148 * r - 0.291 * g + 0.439 * b + 128.0).clamp(0.0, 255.0) as u8;
            uv_plane[(row / 2) * width + col + 1] =
                (0.439 * r - 0.368 * g - 0.071 * b + 128.0).clamp(0.0, 255.0) as u8;
        }
    }
    (y_plane, uv_plane)
}

/// MFT H.264 encoders are allowed to return either Annex-B or AVC
/// length-prefixed NAL units. Normalize both forms before handing bytes to
/// VideoToolbox. This is important: feeding AVC bytes as Annex-B produces a
/// decoder that stays black without reporting a useful error.
fn to_annex_b(data: &[u8]) -> Vec<u8> {
    if data.windows(4).any(|w| w == [0, 0, 0, 1]) || data.windows(3).any(|w| w == [0, 0, 1]) {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len() + 32);
    let mut pos = 0;
    while pos + 4 <= data.len() {
        let len = u32::from_be_bytes([data[pos], data[pos+1], data[pos+2], data[pos+3]]) as usize;
        pos += 4;
        if len == 0 || pos + len > data.len() { return data.to_vec(); }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[pos..pos + len]);
        pos += len;
    }
    if pos != data.len() || out.is_empty() { data.to_vec() } else { out }
}

unsafe fn fill_buffer(buf: &IMFMediaBuffer, data: &[u8]) -> Result<(), windows::core::Error> {
    let mut ptr = std::ptr::null_mut();
    let mut max_len = 0u32;
    buf.Lock(&mut ptr, Some(&mut max_len), None)?;
    let dst = std::slice::from_raw_parts_mut(ptr as *mut u8, data.len());
    dst.copy_from_slice(data);
    buf.Unlock()?;
    let _ = buf.SetCurrentLength(data.len() as u32);
    Ok(())
}

unsafe fn read_sample_data(sample: &IMFSample) -> Result<Vec<u8>, windows::core::Error> {
    let count = sample.GetBufferCount()?;
    let mut out = Vec::new();
    for i in 0..count {
        let buf = sample.GetBufferByIndex(i)?;
        let mut ptr = std::ptr::null_mut();
        let mut max_len = 0u32;
        buf.Lock(&mut ptr, Some(&mut max_len), None)?;
        let len = buf.GetCurrentLength().unwrap_or(max_len);
        let slice = std::slice::from_raw_parts(ptr as *const u8, len as usize);
        out.extend_from_slice(slice);
        buf.Unlock()?;
    }
    Ok(out)
}

/// Block until the next event of the desired type arrives.
struct EventState {
    events: Mutex<VecDeque<Result<i32, String>>>,
    ready: Condvar,
}

#[implement(IMFAsyncCallback)]
struct MftEventCallback {
    generator: IMFMediaEventGenerator,
    state: Arc<EventState>,
}

impl IMFAsyncCallback_Impl for MftEventCallback_Impl {
    fn GetParameters(&self, flags: *mut u32, queue: *mut u32) -> windows::core::Result<()> {
        unsafe {
            *flags = 0;
            *queue = 0;
        }
        Ok(())
    }

    fn Invoke(&self, result: Ref<'_, IMFAsyncResult>) -> windows::core::Result<()> {
        let value = match result.ok() {
            Ok(result) => match unsafe { self.generator.EndGetEvent(result) } {
                Ok(event) => unsafe { event.GetType() }
                    .map(|kind| kind as i32)
                    .map_err(|e| e.to_string()),
                Err(error) => Err(error.to_string()),
            },
            Err(error) => Err(error.to_string()),
        };
        self.state.events.lock().unwrap().push_back(value);
        self.state.ready.notify_one();
        Ok(())
    }
}

impl MftEncoder {
    /// Completes the output-type handshake requested by an MFT through
    /// MF_E_TRANSFORM_STREAM_CHANGE. Hardware drivers commonly defer this
    /// until they have accepted the first input sample.
    fn renegotiate_output_type(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let transform = self
            .transform
            .as_ref()
            .ok_or("encoder not initialized")?
            .clone();
        unsafe {
            let output_type = transform
                .GetOutputAvailableType(self.output_stream_id, 0)
                .map_err(|e| format!("GetOutputAvailableType after stream change: {e}"))?;
            transform
                .SetOutputType(self.output_stream_id, &output_type, 0)
                .map_err(|e| format!("SetOutputType after stream change: {e}"))?;
            let output_info = transform
                .GetOutputStreamInfo(self.output_stream_id)
                .map_err(|e| format!("GetOutputStreamInfo after stream change: {e}"))?;
            self.output_provides_samples = output_info.dwFlags
                & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            self.output_buf_size = output_info
                .cbSize
                .max(self.width.saturating_mul(self.height).saturating_mul(2));
        }
        println!(
            "H.264 MFT output stream renegotiated: {}",
            if self.output_provides_samples {
                "MFT-provided samples"
            } else {
                "caller-provided samples"
            }
        );
        Ok(())
    }

    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        keyframe_interval: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        if width < 2 || height < 2 || width % 2 != 0 || height % 2 != 0 || fps == 0 {
            return Err("MFT encoder requires positive even dimensions and non-zero FPS".into());
        }
        if !mf_runtime_init() {
            return Err("Media Foundation runtime init failed".into());
        }

let activate = match enumerate_hw_h264_encoders() {
              Ok(hw) if !hw.is_empty() => {
                  if DEBUG { eprintln!("[MFT] using hardware H.264 encoder"); }
                  hw.into_iter().next().unwrap()
              }
              _ => {
                  let sw = enumerate_sw_h264_encoders()
                      .map_err(|e| format!("enumerate SW encoders: {e}"))?;
                  if sw.is_empty() {
                      return Err("no H.264 encoder MFT found".into());
                  }
                  if DEBUG { eprintln!("[MFT] using software H.264 encoder"); }
                  sw.into_iter().next().unwrap()
              }
          };

        let transform: IMFTransform = unsafe {
            activate
                .ActivateObject()
                .map_err(|e| format!("ActivateObject: {e}"))?
        };

        let async_mode = unsafe {
            let attrs = transform
                .GetAttributes()
                .map_err(|e| format!("GetAttributes: {e}"))?;
            let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0
        };
        let event_state = Arc::new(EventState { events: Mutex::new(VecDeque::new()), ready: Condvar::new() });
        let (event_gen, event_callback) = if async_mode {
            let generator: IMFMediaEventGenerator = transform
                .cast()
                .map_err(|e| format!("async MFT has no IMFMediaEventGenerator: {e}"))?;
            let callback: IMFAsyncCallback = MftEventCallback {
                generator: generator.clone(),
                state: Arc::clone(&event_state),
            }.into();
            (Some(generator), Some(callback))
        } else {
            (None, None)
        };

        unsafe {
            let output_type: IMFMediaType =
                MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
            output_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|()| output_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264))
                .and_then(|()| output_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_kbps * 1000))
                .and_then(|()| {
                    output_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(width, height))
                })
                .and_then(|()| output_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(fps, 1)))
                .and_then(|()| {
                    output_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u32_pair(1, 1))
                })
                .and_then(|()| {
                    output_type.SetUINT32(
                        &MF_MT_INTERLACE_MODE,
                        MFVideoInterlace_Progressive.0 as u32,
                    )
                })
                .and_then(|()| {
                    output_type
                        .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)
                })
                .and_then(|()| {
                    // Avoid the bandwidth and latency cost of making every
                    // frame an IDR while keeping reconnect time short.
                    output_type.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, keyframe_interval.max(1))
                })
                .map_err(|e| format!("set output type attrs: {e}"))?;
            transform
                .SetOutputType(0, &output_type, 0)
                .map_err(|e| format!("SetOutputType: {e}"))?;
        }

        unsafe {
            let input_type: IMFMediaType =
                MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
            input_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|()| input_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12))
                .and_then(|()| {
                    input_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(width, height))
                })
                .and_then(|()| input_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(fps, 1)))
                .and_then(|()| {
                    input_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u32_pair(1, 1))
                })
                .and_then(|()| {
                    input_type.SetUINT32(
                        &MF_MT_INTERLACE_MODE,
                        MFVideoInterlace_Progressive.0 as u32,
                    )
                })
                .and_then(|()| input_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, width))
                .map_err(|e| format!("set input type attrs: {e}"))?;
            transform
                .SetInputType(0, &input_type, 0)
                .map_err(|e| format!("SetInputType: {e}"))?;
        }

        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| format!("BEGIN_STREAMING: {e}"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| format!("START_OF_STREAM: {e}"))?;
        }

        let output_info = unsafe { transform.GetOutputStreamInfo(0) }
            .map_err(|e| format!("GetOutputStreamInfo: {e}"))?;
        let output_provides_samples = output_info.dwFlags
            & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        // A few hardware MFTs report cbSize=0 despite requiring a caller
        // buffer. Keep a conservative fallback large enough for an IDR.
        let output_buf_size = output_info
            .cbSize
            .max(width.saturating_mul(height).saturating_mul(2));

        // Register only for MFTs that explicitly selected the asynchronous
        // processing contract. A successful COM cast alone does not mean an
        // encoder emits METransformNeedInput/METransformHaveOutput.
        if let (Some(generator), Some(callback)) = (&event_gen, &event_callback) {
            unsafe { generator.BeginGetEvent(callback, None::<&windows::core::IUnknown>)?; }
        }
        println!("H.264 MFT negotiated: mode={}, {}x{} @ {} fps, {} kbps, output={}",
            if async_mode { "async" } else { "sync" }, width, height, fps, bitrate_kbps,
            if output_provides_samples { "MFT-provided samples" } else { "caller-provided samples" });
        Ok(Self {
            transform: Some(transform),
            event_gen,
            input_stream_id: 0,
            output_stream_id: 0,
            width,
            height,
            fps,
            output_buf_size,
            output_provides_samples,
            async_mode,
            frame_index: 0,
            event_state,
            event_callback,
        })
    }

    fn wait_for_event(&self, expected: i32, timeout: std::time::Duration) -> Result<(), Box<dyn std::error::Error>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let mut guard = self.event_state.events.lock().unwrap();
            while guard.is_empty() && std::time::Instant::now() < deadline {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let (next, _) = self.event_state.ready.wait_timeout(guard, remaining).unwrap();
                guard = next;
            }
            let result = guard.pop_front();
            drop(guard);
            let actual = match result {
                Some(Ok(actual)) => actual,
                Some(Err(error)) => return Err(error.into()),
                None => return Err(format!("timed out waiting for MFT event {expected}").into()),
            };
            if let (Some(generator), Some(callback)) = (&self.event_gen, &self.event_callback) {
                unsafe { generator.BeginGetEvent(callback, None::<&windows::core::IUnknown>)?; }
            }
            if actual == expected { return Ok(()); }
        }
    }

    /// Returns `Ok(None)` only when a synchronous MFT has accepted input but
    /// has not produced an access unit yet. This is normal during its initial
    /// pipeline fill and must not disconnect the interactive stream.
    pub fn encode_frame(&mut self, bgra: &[u8]) -> Result<Option<EncodedFrame>, Box<dyn std::error::Error>> {
        if DEBUG { eprintln!("[MFT] encode_frame called"); }
        let expected = self.width as usize * self.height as usize * 4;
        if bgra.len() != expected {
            return Err(format!("BGRA frame has {} bytes, expected {}", bgra.len(), expected).into());
        }
        let transform = self
            .transform
            .as_ref()
            .ok_or("encoder not initialized")?
            .clone();

        // Convert BGRA -> NV12
        let (y_plane, uv_plane) = bgra_to_nv12(bgra, self.width as usize, self.height as usize);

        // Async MFTs advertise input capacity through METransformNeedInput.
        // Synchronous MFTs instead use the ProcessInput/ProcessOutput contract.
        if self.async_mode {
            self.wait_for_event(METransformNeedInput.0, std::time::Duration::from_secs(1))?;
        }

        // --- 1. Create input sample ---
        let input_sample = unsafe {
            let sample = MFCreateSample().map_err(|e| format!("MFCreateSample: {e}"))?;
            let mut nv12 = y_plane;
            nv12.extend_from_slice(&uv_plane);
            let buf = MFCreateMemoryBuffer(nv12.len() as u32)
                .map_err(|e| format!("MFCreateMemoryBuffer NV12: {e}"))?;
            fill_buffer(&buf, &nv12).map_err(|e| format!("fill NV12: {e}"))?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(self.frame_index * 10_000_000i64 / self.fps as i64)?;
            sample.SetSampleDuration(10_000_000i64 / self.fps as i64)?;
            sample
        };
        self.frame_index += 1;

        // --- 2. ProcessInput ---
        unsafe {
            transform
                .ProcessInput(self.input_stream_id, &input_sample, 0)
                .map_err(|e| format!("ProcessInput: {e}"))?;
        }

        if self.async_mode {
            self.wait_for_event(METransformHaveOutput.0, std::time::Duration::from_secs(1))?;
        }
        let mut output_renegotiations = 0;
        let sample = loop {
            let output_sample = if self.output_provides_samples {
                None
            } else {
                Some(unsafe {
                    let sample = MFCreateSample().map_err(|e| format!("MFCreateSample: {e}"))?;
                    let buf = MFCreateMemoryBuffer(self.output_buf_size)
                        .map_err(|e| format!("MFCreateMemoryBuffer: {e}"))?;
                    sample.AddBuffer(&buf)?;
                    sample
                })
            };
            let mut output_data_buffer = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output_stream_id,
                pSample: ManuallyDrop::new(output_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let mut output_status = 0u32;
            let process_output = unsafe {
                transform.ProcessOutput(
                    0,
                    std::slice::from_mut(&mut output_data_buffer),
                    &mut output_status,
                )
            };
            let sample = unsafe { ManuallyDrop::take(&mut output_data_buffer.pSample) }
                .ok_or("encoder returned no output sample");
            // ProcessOutput owns these ManuallyDrop fields. Take them even on
            // an error so a driver error cannot leak an MFT-owned sample.
            let events: Option<IMFCollection> =
                unsafe { ManuallyDrop::take(&mut output_data_buffer.pEvents) };
            drop(events);
            match process_output {
                Ok(()) => break sample?,
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    output_renegotiations += 1;
                    if output_renegotiations > 2 {
                        return Err("MFT repeatedly requested output stream renegotiation".into());
                    }
                    self.renegotiate_output_type()?;
                    // The same input has an output pending. Per the MFT
                    // contract, set the new type and call ProcessOutput again.
                    // Async MFTs signal readiness again after that handshake;
                    // calling ProcessOutput before the event violates their
                    // processing contract on Intel/NVIDIA drivers.
                    if self.async_mode {
                        self.wait_for_event(
                            METransformHaveOutput.0,
                            std::time::Duration::from_secs(1),
                        )?;
                    }
                }
                Err(error) if !self.async_mode && error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    return Ok(None);
                }
                Err(error) => return Err(format!("ProcessOutput: {error}").into()),
            }
        };
        let got_keyframe = unsafe {
            sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1
        };
        let all_data = unsafe { read_sample_data(&sample) }
            .map_err(|e| format!("read encoded sample: {e}"))
            .map(|data| to_annex_b(&data))?;
        if all_data.is_empty() {
            return Err("no output from encoder".into());
        }

        if DEBUG {
            eprintln!("[MFT] encoded frame: {} bytes, keyframe={}", all_data.len(), got_keyframe);
        }
        Ok(Some(EncodedFrame {
            data: all_data,
            keyframe: got_keyframe,
        }))
    }
}

impl Drop for MftEncoder {
    fn drop(&mut self) {
        if let Some(transform) = self.transform.take() {
            unsafe {
                let _ = transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            }
        }
    }
}

pub fn is_hw_encoder_available() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        if !mf_runtime_init() {
            return false;
        }
        match enumerate_hw_h264_encoders() {
            Ok(hw) => !hw.is_empty(),
            Err(_) => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{bgra_to_nv12, ensure_com_initialized, is_hw_encoder_available, to_annex_b, MftEncoder};
    use crate::capture::{MonitorCapture, MonitorCaptureConfig};
    use std::time::Duration;

    fn annex_b_nal_types(data: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        let mut offset = 0;
        while offset + 3 < data.len() {
            let start_code_len = if data[offset..].starts_with(&[0, 0, 0, 1]) {
                4
            } else if data[offset..].starts_with(&[0, 0, 1]) {
                3
            } else {
                offset += 1;
                continue;
            };
            let nal_start = offset + start_code_len;
            if let Some(header) = data.get(nal_start) {
                types.push(header & 0x1f);
            }
            offset = nal_start;
        }
        types
    }

    #[test]
    fn converts_avc_length_prefixes_to_annex_b() {
        let avc = [0, 0, 0, 3, 0x65, 1, 2, 0, 0, 0, 2, 0x06, 7];
        assert_eq!(to_annex_b(&avc), vec![0,0,0,1,0x65,1,2,0,0,0,1,0x06,7]);
    }

    #[test]
    fn keeps_annex_b_unchanged() {
        let annex_b = [0, 0, 0, 1, 0x67, 1, 2];
        assert_eq!(to_annex_b(&annex_b), annex_b);
    }

    #[test]
    fn bgra_nv12_has_expected_layout_and_neutral_chroma() {
        let (y, uv) = bgra_to_nv12(&[0, 0, 0, 0, 255, 255, 255, 0, 0, 0, 0, 0, 255, 255, 255, 0], 2, 2);
        assert_eq!(y.len(), 4);
        assert_eq!(uv.len(), 2);
        assert!((uv[0] as i32 - 128).abs() <= 1);
        assert!((uv[1] as i32 - 128).abs() <= 1);
    }

    /// Exercises the production WGC -> BGRA/NV12 -> hardware MFT path.  It
    /// deliberately checks for SPS, PPS, and an IDR, the minimum sequence the
    /// iOS VideoToolbox decoder needs before it can render a frame.
    #[test]
    #[ignore = "requires a Windows desktop, a monitor, and a hardware H.264 MFT"]
    fn hardware_mft_smoke_produces_ios_decodable_h264() {
        // Mirror stream::handle_client: a fresh client-worker thread owns COM,
        // WGC, and the non-Send MFT interfaces for its entire lifetime.
        std::thread::spawn(|| {
            assert!(is_hw_encoder_available(), "no hardware H.264 MFT is available");
            ensure_com_initialized();
            let monitor_index = std::env::var("TOUCHMONITOR_TEST_MONITOR")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            let capture = MonitorCapture::start(
                monitor_index,
                MonitorCaptureConfig {
                    buffers: 2,
                    include_cursor: true,
                },
            )
            .expect("start Windows Graphics Capture");

            let mut encoder = None;
            let mut seen_sps = false;
            let mut seen_pps = false;
            let mut seen_idr = false;
            for _ in 0..120 {
                let frame = capture
                    .next_frame_timeout(Duration::from_secs(1))
                    .expect("capture a desktop frame");
                let encoder = encoder.get_or_insert_with(|| {
                    MftEncoder::new(frame.width, frame.height, 60, 8_000, 1)
                        .expect("initialize hardware H.264 MFT")
                });
                let Some(encoded) = encoder.encode_frame(&frame.bgra).expect("encode captured frame") else {
                    continue;
                };
                assert!(!encoded.data.is_empty(), "MFT returned an empty access unit");
                let types = annex_b_nal_types(&encoded.data);
                seen_sps |= types.contains(&7);
                seen_pps |= types.contains(&8);
                seen_idr |= types.contains(&5);
                if seen_sps && seen_pps && seen_idr {
                    return;
                }
            }
            panic!(
                "hardware MFT did not emit SPS/PPS/IDR within 120 captured frames \
                 (sps={seen_sps}, pps={seen_pps}, idr={seen_idr})"
            );
        })
        .join()
        .expect("hardware MFT smoke worker panicked");
    }

    /// Runs the actual hardware MFT without requiring access to the interactive
    /// desktop. This is the fallback CI/agent check for the MFT contract;
    /// `hardware_mft_smoke_produces_ios_decodable_h264` additionally verifies
    /// the WGC source when it is run from the logged-in desktop session.
    #[test]
    #[ignore = "requires a hardware H.264 MFT"]
    fn hardware_mft_encodes_synthetic_access_unit() {
        assert!(is_hw_encoder_available(), "no hardware H.264 MFT is available");
        ensure_com_initialized();
        let width = 1280;
        let height = 720;
        let bgra = vec![0x40; width as usize * height as usize * 4];
        let mut encoder = MftEncoder::new(width, height, 60, 8_000, 1)
            .expect("initialize hardware H.264 MFT");
        let mut seen_sps = false;
        let mut seen_pps = false;
        let mut seen_idr = false;
        for _ in 0..8 {
            if let Some(encoded) = encoder.encode_frame(&bgra).expect("encode synthetic frame") {
                assert!(!encoded.data.is_empty(), "MFT returned an empty access unit");
                let types = annex_b_nal_types(&encoded.data);
                seen_sps |= types.contains(&7);
                seen_pps |= types.contains(&8);
                seen_idr |= types.contains(&5);
                if seen_sps && seen_pps && seen_idr {
                    return;
                }
            }
        }
        panic!(
            "hardware MFT did not emit SPS/PPS/IDR within eight frames \
             (sps={seen_sps}, pps={seen_pps}, idr={seen_idr})"
        );
    }
}
