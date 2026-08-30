//! Screen capture using the Windows Graphics Capture (WGC) / DXGI pipeline.
//!
//! A free-threaded `Direct3D11CaptureFramePool` delivers BGRA8 frames on an
//! internal thread-pool thread via `FrameArrived`; we copy each frame to host
//! memory and push it (as `(Vec<u8>, w, h)`) into a bounded channel. A separate
//! consumer (the encoder) drains that channel.
//!
//! Only pulls in the `windows` crate's WinRT Graphics Capture projections; no
//! external native dependencies, so it cross-compiles cleanly in CI.

use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use windows::core::{factory, BOOL, Interface, IInspectable};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::{
    Capture::{
        Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
    },
    DirectX::{
        Direct3D11::IDirect3DDevice,
        DirectXPixelFormat,
    },
};
use windows::Win32::{
    Foundation::{LPARAM, RECT},
    Graphics::{
        Direct3D::D3D_DRIVER_TYPE_HARDWARE,
        Direct3D11::{
            D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CPU_ACCESS_READ,
            D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
            D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
        },
        Dxgi::IDXGIDevice,
        Gdi::{
            EnumDisplayMonitors, GetMonitorInfoW, HMONITOR, MONITORINFO,
        },
    },
    System::WinRT::{
        Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
        Graphics::Capture::IGraphicsCaptureItemInterop,
    },
    UI::WindowsAndMessaging::MONITORINFOF_PRIMARY,
};

/// One captured frame: contiguous BGRA8 pixels (bottom-left origin), top-down copied.
#[derive(Clone)]
pub struct CapturedFrame {
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Bounds of a monitor in virtual-screen pixel space (top-left origin).
#[derive(Clone, Copy, Debug)]
pub struct MonitorBounds {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub hmonitor: HMONITOR,
    pub is_primary: bool,
}

/// Lists connected monitors and their sizes.
pub fn list_monitors() -> Result<Vec<MonitorBounds>, Box<dyn std::error::Error>> {
    let monitors = enumerate_monitors();
    let mut out = Vec::new();
    for (hmon, bounds, is_primary) in monitors {
        out.push(MonitorBounds {
            left: bounds.left,
            top: bounds.top,
            width: (bounds.right - bounds.left) as u32,
            height: (bounds.bottom - bounds.top) as u32,
            hmonitor: hmon,
            is_primary,
        });
    }
    Ok(out)
}

fn enumerate_monitors() -> Vec<(HMONITOR, RECT, bool)> {
    let mut result: Vec<(HMONITOR, RECT, bool)> = Vec::new();
    unsafe {
        let data = (&mut result as *mut Vec<(HMONITOR, RECT, bool)>) as isize;
        let _ = EnumDisplayMonitors(None, None, Some(enum_monitor_proc), LPARAM(data));
    }
    result
}

unsafe extern "system" fn enum_monitor_proc(
    hmonitor: HMONITOR,
    _hdc: windows::Win32::Graphics::Gdi::HDC,
    _lprcclip: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let monitors = &mut *(data.0 as *mut Vec<(HMONITOR, RECT, bool)>);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let is_primary = unsafe { GetMonitorInfoW(hmonitor, &mut info) }.as_bool()
        && (info.dwFlags & MONITORINFOF_PRIMARY) != 0;
    monitors.push((hmonitor, info.rcMonitor, is_primary));
    BOOL(1)
}

/// Continuous monitor capture context.
pub struct MonitorCapture {
    _session: GraphicsCaptureSession,
    _pool: Direct3D11CaptureFramePool,
    _item: GraphicsCaptureItem,
    rx: Receiver<CapturedFrame>,
    running: Arc<AtomicBool>,
    bounds: MonitorBounds,
}

pub struct MonitorCaptureConfig {
    pub buffers: i32,
    pub include_cursor: bool,
}

impl Default for MonitorCaptureConfig {
    fn default() -> Self {
        Self {
            buffers: 2,
            include_cursor: true,
        }
    }
}

impl MonitorCapture {
    pub fn start(
        monitor_index: usize,
        cfg: MonitorCaptureConfig,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let monitors = list_monitors()?;
        let bounds = *monitors
            .get(monitor_index)
            .ok_or_else(|| format!("monitor index {monitor_index} out of range ({} total)", monitors.len()))?;

        let (device, context) = create_d3d_device()?;
        let dxgi_device: IDXGIDevice = device.cast()?;
        let rt_device: IDirect3DDevice =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) }?
                .cast()?;

        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(bounds.hmonitor) }?;
        let item_size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &rt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            cfg.buffers,
            item_size,
        )?;

        let (tx, rx) = sync_channel::<CapturedFrame>(cfg.buffers as usize);
        let running = Arc::new(AtomicBool::new(true));

        let handler_dev = device.clone();
        let handler_ctx = context.clone();

        let _frame_arrived_token = pool.FrameArrived(
            &TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
                move |pool, _| {
                    let Some(pool) = pool.as_ref() else {
                        return Ok(());
                    };
                    let Ok(frame) = pool.TryGetNextFrame() else {
                        return Ok(());
                    };
                    let result = extract_frame(&handler_dev, &handler_ctx, &frame);
                    let _ = frame.Close();
                    if let Ok(captured) = result {
                        // Drop frames if the consumer is backed up.
                        let _ = tx.try_send(captured);
                    }
                    Ok(())
                },
            ),
        )?;

        let session = pool.CreateCaptureSession(&item)?;
        // Disable the yellow capture border so it doesn't appear in the stream.
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetIsCursorCaptureEnabled(cfg.include_cursor);
        session.StartCapture()?;

        Ok(Self {
            _session: session,
            _pool: pool,
            _item: item,
            rx,
            running,
            bounds,
        })
    }

    pub fn bounds(&self) -> MonitorBounds {
        self.bounds
    }

    /// Block for up to `timeout` for the next frame, or return None on timeout.
    pub fn next_frame(&self) -> Result<CapturedFrame, CaptureError> {
        self.rx
            .recv()
            .map_err(|_| CaptureError::Closed("frame channel closed"))
    }

    pub fn next_frame_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<CapturedFrame, CaptureError> {
        use std::sync::mpsc::RecvTimeoutError;
        match self.rx.recv_timeout(timeout) {
            Ok(f) => Ok(f),
            Err(RecvTimeoutError::Timeout) => Err(CaptureError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(CaptureError::Closed("frame channel closed")),
        }
    }

    pub fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub enum CaptureError {
    Timeout,
    Closed(&'static str),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::Timeout => write!(f, "capture timeout"),
            CaptureError::Closed(msg) => write!(f, "capture closed: {msg}"),
        }
    }
}
impl std::error::Error for CaptureError {}

fn create_d3d_device() -> Result<(ID3D11Device, ID3D11DeviceContext), Box<dyn std::error::Error>> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let device = device.ok_or("no d3d11 device")?;
    let context = context.ok_or("no d3d11 context")?;
    Ok((device, context))
}

fn extract_frame(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
) -> Result<CapturedFrame, String> {
    let surface = frame
        .Surface()
        .map_err(|e| format!("frame surface: {e}"))?;
    let access: IDirect3DDxgiInterfaceAccess = surface
        .cast()
        .map_err(|e| format!("dxgi access: {e}"))?;
    let texture: ID3D11Texture2D = unsafe { access.GetInterface() }
        .map_err(|e| format!("get texture: {e}"))?;

    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };

    let mut staging_desc = desc;
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.BindFlags = 0;
    staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    staging_desc.MiscFlags = 0;

    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }
        .map_err(|e| format!("staging texture: {e}"))?;
    let staging = staging.ok_or_else(|| "no staging texture".to_string())?;

    unsafe { context.CopyResource(&staging, &texture) };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
        .map_err(|e| format!("map: {e}"))?;

    let width = desc.Width;
    let height = desc.Height;

    // Crop to content size (avoids padding).
    let content = frame.ContentSize().map_err(|e| format!("content size: {e}"))?;
    let content_w = (content.Width.max(0) as u32).min(width);
    let content_h = (content.Height.max(0) as u32).min(height);

    if content_w == 0 || content_h == 0 {
        unsafe { context.Unmap(&staging, 0) };
        return Err("captured content size is zero".to_string());
    }

    let row_pitch = mapped.RowPitch as usize;
    let src = mapped.pData as *const u8;
    let mut bgra = Vec::with_capacity(content_w as usize * content_h as usize * 4);
    for y in 0..content_h as usize {
        let src_row = unsafe { src.add(y * row_pitch) };
        let row = unsafe { std::slice::from_raw_parts(src_row, content_w as usize * 4) };
        bgra.extend_from_slice(row);
    }
    unsafe { context.Unmap(&staging, 0) };

    Ok(CapturedFrame {
        bgra,
        width: content_w,
        height: content_h,
    })
}
