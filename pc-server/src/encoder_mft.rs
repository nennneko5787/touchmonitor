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
    frame_index: i64,
    event_state: Arc<EventState>,
    event_callback: IMFAsyncCallback,
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
    event: Mutex<Option<Result<i32, String>>>,
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
        *self.state.event.lock().unwrap() = Some(value);
        self.state.ready.notify_one();
        Ok(())
    }
}

impl MftEncoder {
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

        let event_gen: IMFMediaEventGenerator = transform
            .cast()
            .map_err(|e| format!("cast to IMFMediaEventGenerator: {e}"))?;
        let event_state = Arc::new(EventState { event: Mutex::new(None), ready: Condvar::new() });
        let event_callback: IMFAsyncCallback = MftEventCallback {
            generator: event_gen.clone(),
            state: Arc::clone(&event_state),
        }.into();

        unsafe {
            let attrs = transform
                .GetAttributes()
                .map_err(|e| format!("GetAttributes: {e}"))?;
            let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
        }

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

        // START_OF_STREAM causes an async encoder to queue its first
        // METransformNeedInput event. Register the callback immediately after
        // starting the stream so that event is delivered by the MFT queue.
        unsafe { event_gen.BeginGetEvent(&event_callback, None::<&windows::core::IUnknown>)?; }

if DEBUG {
            eprintln!("[MFT] encoder initialized: {}x{} @ {}fps, {}kbps", width, height, fps, bitrate_kbps);
        }
        Ok(Self {
            transform: Some(transform),
            event_gen: Some(event_gen),
            input_stream_id: 0,
            output_stream_id: 0,
            width,
            height,
            fps,
            output_buf_size: width * height * 2,
            frame_index: 0,
            event_state,
            event_callback,
        })
    }

    fn wait_for_event(&self, expected: i32, timeout: std::time::Duration) -> Result<(), Box<dyn std::error::Error>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let mut guard = self.event_state.event.lock().unwrap();
            while guard.is_none() && std::time::Instant::now() < deadline {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let (next, _) = self.event_state.ready.wait_timeout(guard, remaining).unwrap();
                guard = next;
            }
            let result = guard.take();
            drop(guard);
            let actual = match result {
                Some(Ok(actual)) => actual,
                Some(Err(error)) => return Err(error.into()),
                None => return Err(format!("timed out waiting for MFT event {expected}").into()),
            };
            unsafe { self.event_gen.as_ref().unwrap().BeginGetEvent(&self.event_callback, None::<&windows::core::IUnknown>)?; }
            if actual == expected { return Ok(()); }
        }
    }

    pub fn encode_frame(&mut self, bgra: &[u8]) -> Result<EncodedFrame, Box<dyn std::error::Error>> {
        if DEBUG { eprintln!("[MFT] encode_frame called"); }
        let expected = self.width as usize * self.height as usize * 4;
        if bgra.len() != expected {
            return Err(format!("BGRA frame has {} bytes, expected {}", bgra.len(), expected).into());
        }
        let transform = self
            .transform
            .as_ref()
            .ok_or("encoder not initialized")?;

        // Convert BGRA -> NV12
        let (y_plane, uv_plane) = bgra_to_nv12(bgra, self.width as usize, self.height as usize);

        // Async MFTs advertise input capacity through METransformNeedInput.
        // Wait with a bound so a broken driver cannot hang the server forever.
        self.wait_for_event(METransformNeedInput.0, std::time::Duration::from_secs(1))?;

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

        // Async MFT output is signalled through the same callback. Do not mix
        // GetEvent polling with BeginGetEvent; polling consumes/reorders events.
        self.wait_for_event(METransformHaveOutput.0, std::time::Duration::from_secs(1))?;
        let output_sample = unsafe {
            let sample = MFCreateSample().map_err(|e| format!("MFCreateSample: {e}"))?;
            let buf = MFCreateMemoryBuffer(self.output_buf_size)
                .map_err(|e| format!("MFCreateMemoryBuffer: {e}"))?;
            sample.AddBuffer(&buf)?;
            sample
        };
        let mut output_data_buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.output_stream_id,
            pSample: ManuallyDrop::new(Some(output_sample)),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut output_status = 0u32;
        unsafe {
            transform.ProcessOutput(
                0,
                std::slice::from_mut(&mut output_data_buffer),
                &mut output_status,
            ).map_err(|e| format!("ProcessOutput: {e}"))?;
        }
        let sample = unsafe { ManuallyDrop::take(&mut output_data_buffer.pSample) }
            .ok_or("encoder returned no output sample")?;
        let got_keyframe = unsafe {
            sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1
        };
        let all_data = unsafe { read_sample_data(&sample) }
            .map_err(|e| format!("read encoded sample: {e}"))
            .map(|data| to_annex_b(&data))?;
        let events: Option<IMFCollection> =
            unsafe { ManuallyDrop::take(&mut output_data_buffer.pEvents) };
        drop(events);

        if all_data.is_empty() {
            return Err("no output from encoder".into());
        }

        if DEBUG {
            eprintln!("[MFT] encoded frame: {} bytes, keyframe={}", all_data.len(), got_keyframe);
        }
        Ok(EncodedFrame {
            data: all_data,
            keyframe: got_keyframe,
        })
    }
}

impl Drop for MftEncoder {
    fn drop(&mut self) {
        if let Some(transform) = self.transform.take() {
            unsafe {
                let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                let _ = transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
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
    use super::{bgra_to_nv12, to_annex_b};

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
}
