//! GPU-accelerated H.264 encoder using Windows Media Foundation (MFT).
//!
//! Hardware MFTs (Intel QSV, AMD VCN, NVIDIA NVENC) are async. We use
//! IMFMediaEventGenerator for METransformNeedInput / METransformHaveOutput
//! events, but with a simpler polling-based drain loop.
//!
//! Input: BGRA8 frames (from Windows Graphics Capture).
//! Output: Annex-B H.264 access units.

use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::OnceLock;
use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

const LOG_ENABLED: bool = false; // Set to true to enable verbose logging

pub struct MftEncoder {
    transform: Option<IMFTransform>,
    event_gen: Option<IMFMediaEventGenerator>,
    input_stream_id: u32,
    output_stream_id: u32,
    width: u32,
    height: u32,
    output_buf_size: u32,
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
             Err(e) => {
                 if LOG_ENABLED {
                     eprintln!("[MFT] MFStartup failed: {e}");
                 }
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
            MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0),
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
fn wait_for_event(
    event_gen: &IMFMediaEventGenerator,
    expected: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let event = unsafe { event_gen.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))? };
        let event_type = unsafe { event.GetType()? } as i32;
        if event_type == expected {
            return Ok(());
        }
        // Unexpected event — keep waiting
    }
}

impl MftEncoder {
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        if !mf_runtime_init() {
            return Err("Media Foundation runtime init failed".into());
        }

let activate = match enumerate_hw_h264_encoders() {
             Ok(hw) if !hw.is_empty() => {
                 if LOG_ENABLED {
                     eprintln!("[MFT] using hardware H.264 encoder");
                 }
                 hw.into_iter().next().unwrap()
             }
             _ => {
                 let sw = enumerate_sw_h264_encoders()
                     .map_err(|e| format!("enumerate SW encoders: {e}"))?;
                 if sw.is_empty() {
                     return Err("no H.264 encoder MFT found".into());
                 }
                 if LOG_ENABLED {
                     eprintln!("[MFT] using software H.264 encoder");
                 }
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
                    // Keyframe every frame (IDR only) to ensure immediate decodability.
                    // This solves the "black on connect" issue by guaranteeing the first frame
                    // is an IDR with SPS/PPS. Adjust to a higher value (e.g., fps) for better
                    // compression once streaming is stable.
                    output_type.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, 1)
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

        eprintln!(
            "[MFT] encoder initialized: {}x{} @ {}fps, {}kbps",
            width, height, fps, bitrate_kbps
        );

        Ok(Self {
            transform: Some(transform),
            event_gen: Some(event_gen),
            input_stream_id: 0,
            output_stream_id: 0,
            width,
            height,
            output_buf_size: width * height * 2,
        })
    }

    pub fn encode_frame(&mut self, bgra: &[u8]) -> Result<EncodedFrame, Box<dyn std::error::Error>> {
        eprintln!("[MFT] encode_frame called");
        let transform = self
            .transform
            .as_ref()
            .ok_or("encoder not initialized")?;
        let event_gen = self
            .event_gen
            .as_ref()
            .ok_or("event generator not initialized")?;

        // Convert BGRA -> NV12
        let (y_plane, uv_plane) = bgra_to_nv12(bgra, self.width as usize, self.height as usize);
        eprintln!("[MFT] BGRA->NV12 conversion done");

        // --- 1. Wait for METransformNeedInput (blocking) ---
        eprintln!("[MFT] waiting for METransformNeedInput");
        wait_for_event(event_gen, METransformNeedInput.0)?;
        eprintln!("[MFT] got METransformNeedInput");

        // --- 2. Create input sample ---
        let input_sample = unsafe {
            let sample = MFCreateSample().map_err(|e| format!("MFCreateSample: {e}"))?;
            let y_buf = MFCreateMemoryBuffer(y_plane.len() as u32)
                .map_err(|e| format!("MFCreateMemoryBuffer Y: {e}"))?;
            fill_buffer(&y_buf, &y_plane).map_err(|e| format!("fill Y: {e}"))?;
            sample.AddBuffer(&y_buf)?;
            let uv_buf = MFCreateMemoryBuffer(uv_plane.len() as u32)
                .map_err(|e| format!("MFCreateMemoryBuffer UV: {e}"))?;
            fill_buffer(&uv_buf, &uv_plane).map_err(|e| format!("fill UV: {e}"))?;
            sample.AddBuffer(&uv_buf)?;
            sample
        };
        eprintln!("[MFT] input sample created");

        // --- 3. ProcessInput ---
        eprintln!("[MFT] calling ProcessInput");
        unsafe {
            transform
                .ProcessInput(self.input_stream_id, &input_sample, 0)
                .map_err(|e| format!("ProcessInput: {e}"))?;
        }
        eprintln!("[MFT] ProcessInput succeeded");

        // --- 4. Drain ALL available output (may be multiple frames) ---
        let mut all_data = Vec::new();
        let mut got_keyframe = false;

        eprintln!("[MFT] entering output drain loop");
        loop {
            // Non-blocking poll for METransformHaveOutput
            let event = unsafe {
                match event_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                    Ok(e) => e,
                    Err(_) => break, // No more events, done
                }
            };

            let event_type = unsafe { event.GetType().unwrap_or(0) };

            if event_type as i32 == METransformHaveOutput.0 {
                eprintln!("[MFT] got METransformHaveOutput event");
                // Process output
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
                match unsafe {
                    transform.ProcessOutput(
                        0,
                        std::slice::from_mut(&mut output_data_buffer),
                        &mut output_status,
                    )
                } {
                    Ok(()) => {
                        eprintln!("[MFT] ProcessOutput succeeded");
                        let sample_opt: Option<IMFSample> =
                            unsafe { ManuallyDrop::take(&mut output_data_buffer.pSample) };
                        if let Some(sample) = sample_opt {
                            let flags = unsafe {
                                sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0)
                            };
                            if flags == 1 {
                                got_keyframe = true;
                                eprintln!("[MFT] output frame is keyframe");
                            } else {
                                eprintln!("[MFT] output frame is not keyframe (flags={})", flags);
                            }
                            match unsafe { read_sample_data(&sample) } {
                                Ok(data) => {
                                    eprintln!("[MFT] read {} bytes from output sample", data.len());
                                    all_data.extend_from_slice(&data);
                                }
                                Err(e) => eprintln!("[MFT] read_sample: {e}"),
                            }
                        }
                    }
                    Err(hr) => {
                        if hr.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                            eprintln!("[MFT] ProcessOutput returned MF_E_TRANSFORM_NEED_MORE_INPUT");
                            break;
                        }
                        eprintln!("[MFT] ProcessOutput failed with hr={:x}", hr.code().0);
                        break;
                    }
                }

                let events: Option<IMFCollection> =
                    unsafe { ManuallyDrop::take(&mut output_data_buffer.pEvents) };
                drop(events);
            } else {
                eprintln!("[MFT] non-HaveOutput event type={}, breaking", event_type);
                // Not a HaveOutput event; requeue logic not needed as we're polling
                break;
            }
        }
        eprintln!("[MFT] exited output drain loop, got {} bytes", all_data.len());

        // If no output this frame, wait for the next HaveOutput event and process it
        if all_data.is_empty() {
            eprintln!("[MFT] no output yet, waiting for METransformHaveOutput");
            wait_for_event(event_gen, METransformHaveOutput.0)?;
            eprintln!("[MFT] got METransformHaveOutput after wait");

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
            match unsafe {
                transform.ProcessOutput(
                    0,
                    std::slice::from_mut(&mut output_data_buffer),
                    &mut output_status,
                )
            } {
                Ok(()) => {
                    eprintln!("[MFT] ProcessOutput (fallback) succeeded");
                    let sample_opt: Option<IMFSample> =
                        unsafe { ManuallyDrop::take(&mut output_data_buffer.pSample) };
                    if let Some(sample) = sample_opt {
                        let flags = unsafe {
                            sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0)
                        };
                        if flags == 1 {
                            got_keyframe = true;
                            eprintln!("[MFT] fallback output frame is keyframe");
                        } else {
                            eprintln!("[MFT] fallback output frame is not keyframe (flags={})", flags);
                        }
                        match unsafe { read_sample_data(&sample) } {
                            Ok(data) => {
                                eprintln!("[MFT] read {} bytes from fallback output sample", data.len());
                                all_data.extend_from_slice(&data);
                            }
                            Err(e) => eprintln!("[MFT] read_sample fallback: {e}"),
                        }
                    }
                }
                Err(e) => return Err(format!("ProcessOutput fallback: {e}").into()),
            }

            let events: Option<IMFCollection> =
                unsafe { ManuallyDrop::take(&mut output_data_buffer.pEvents) };
            drop(events);
            eprintln!("[MFT] after fallback wait, got {} bytes", all_data.len());
        }

        if all_data.is_empty() {
            return Err("no output from encoder".into());
        }

        eprintln!("[MFT] encoded frame: {} bytes, keyframe={}", all_data.len(), got_keyframe);
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