//! Multi-touch injection into Windows via the user32 `InitializeTouchInjection` /
//! `InjectTouchInput` API. This is the only OS-level way to deliver true multi-touch
//! (as opposed to a single synthetic mouse pointer).
//!
//! The `windows` crate does not project these two functions, and the MSVC import
//! library does not reliably expose `InjectTouchInput`'s import thunk, so we resolve
//! them at runtime with `GetProcAddress`. Because this server is built as a console
//! app (which does not auto-link user32.dll), we must `LoadLibraryW("user32.dll")`
//! explicitly before resolving the entry points.

use std::sync::Mutex;

/// `POINT` (windef.h): a coordinate pair of 32-bit signed integers.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
}

/// `RECT` (windef.h): four 32-bit signed integers.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

/// `POINTER_INFO` (winuser.h): basic pointer information common to all pointer types.
/// Layout must match the OS definition for the current (64-bit) build exactly.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PointerInfo {
    pointer_type: u32,   // POINTER_INPUT_TYPE (PT_TOUCH etc.)
    pointer_id: u32,
    frame_id: u32,
    pointer_flags: u32,  // POINTER_FLAG_*
    source_device: usize, // HANDLE
    hwnd_target: usize,   // HWND
    pt_pixel_location: Point,
    pt_himetric_location: Point,
    pt_pixel_location_raw: Point,
    pt_himetric_location_raw: Point,
    dw_time: u32,
    history_count: u32,
    input_data: i32,
    dw_key_states: u32,
    performance_count: u64,
    button_change_type: i32, // POINTER_BUTTON_CHANGE_TYPE
}

/// `POINTER_TOUCH_INFO` (winuser.h): touch data on top of the pointer header.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PointerTouchInfo {
    pointer_info: PointerInfo,
    touch_flags: u32,      // TOUCH_FLAGS
    touch_mask: u32,       // TOUCH_MASK
    rc_contact: Rect,
    rc_contact_raw: Rect,
    orientation: u32,
    pressure: u32,
}

/// `POINTER_TYPE_INFO` (winuser.h): pointer type + union of per-type info.
/// For PT_TOUCH the union is `POINTER_TOUCH_INFO` (144 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PointerTypeInfo {
    pointer_type: u32,
    touch_info: PointerTouchInfo,
}

type CreateDeviceFn = unsafe extern "system" fn(
    pointer_type: u32,
    max_count: u32,
    feedback_mode: u32,
    hmonitor: usize,
    device_width: u32,
    device_height: u32,
    options: u32,
) -> usize;
type InjectSyntheticFn = unsafe extern "system" fn(
    device: usize,
    pointer_info: *const PointerTypeInfo,
    count: u32,
) -> i32;
type DestroyDeviceFn = unsafe extern "system" fn(device: usize) -> i32;

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(lp_file_name: *const u16) -> *mut core::ffi::c_void;
    fn GetProcAddress(h_module: *mut core::ffi::c_void, lp_proc_name: *const u8) -> *mut core::ffi::c_void;
    fn GetLastError() -> u32;
}

// POINTER_FLAG_* (winuser.h)
const POINTER_FLAG_INRANGE: u32 = 0x0000_0002;
const POINTER_FLAG_INCONTACT: u32 = 0x0000_0004;
const POINTER_FLAG_DOWN: u32 = 0x0001_0000;
const POINTER_FLAG_UPDATE: u32 = 0x0002_0000;
const POINTER_FLAG_UP: u32 = 0x0004_0000;

/// POINTER_INPUT_TYPE: PT_TOUCH
const PT_TOUCH: u32 = 0x0000_0002;

// POINTER_FEEDBACK_MODE
const POINTER_FEEDBACK_NONE: u32 = 0x2;

// SYNTHETIC_DEVICE_CREATION_OPTIONS
const SDCO_NONE: u32 = 0x0;

// TOUCH_MASK_* (winuser.h)
const TOUCH_MASK_CONTACTAREA: u32 = 0x0000_0001;

// TOUCH_FLAG_NONE
const TOUCH_FLAG_NONE: u32 = 0x0000_0000;

/// Contact radius for an injected finger, in pixels.
const FINGER_RADIUS: i32 = 8;

/// Dynamically-resolved user32 synthetic pointer (touch) injection entry points.
struct TouchApi {
    create: CreateDeviceFn,
    inject: InjectSyntheticFn,
    destroy: DestroyDeviceFn,
}

impl TouchApi {
    fn resolve() -> Option<Self> {
        let name: Vec<u16> = "user32\0".encode_utf16().collect();
        // LoadLibraryW guarantees user32 is actually mapped (GetModuleHandleW only
        // returns already-loaded modules, which can be NULL for a console app).
        let user32 = unsafe { LoadLibraryW(name.as_ptr()) };
        if user32.is_null() {
            println!("touch: failed to load user32.dll");
            return None;
        }
        let create_ptr = unsafe { GetProcAddress(user32, b"CreateSyntheticPointerDevice\0".as_ptr()) };
        let inject_ptr = unsafe { GetProcAddress(user32, b"InjectSyntheticPointerInput\0".as_ptr()) };
        let destroy_ptr = unsafe { GetProcAddress(user32, b"DestroySyntheticPointerDevice\0".as_ptr()) };
        if create_ptr.is_null() {
            println!("touch: CreateSyntheticPointerDevice not found");
            return None;
        }
        if inject_ptr.is_null() {
            println!("touch: InjectSyntheticPointerInput not found");
            return None;
        }
        if destroy_ptr.is_null() {
            println!("touch: DestroySyntheticPointerDevice not found");
            return None;
        }
        let create = unsafe { std::mem::transmute::<*mut core::ffi::c_void, CreateDeviceFn>(create_ptr) };
        let inject = unsafe { std::mem::transmute::<*mut core::ffi::c_void, InjectSyntheticFn>(inject_ptr) };
        let destroy = unsafe { std::mem::transmute::<*mut core::ffi::c_void, DestroyDeviceFn>(destroy_ptr) };
        Some(Self { create, inject, destroy })
    }
}

/// Maps normalized (0..1) stream coordinates back into monitor-relative pixel space
/// (0,0 = top-left of the target monitor) and locates the target monitor handle for
/// `CreateSyntheticPointerDevice`.
#[derive(Clone, Copy)]
pub struct ScreenMapping {
    pub width: u32,
    pub height: u32,
    /// HMONITOR handle of the captured monitor (target for the synthetic device).
    pub hmonitor: usize,
}

#[derive(Debug)]
pub enum TouchError {
    Unsupported,
    SendFailed,
}

pub struct TouchInjector {
    api: Option<TouchApi>,
    /// HSYNTHETICPOINTERDEVICE bound to the captured monitor (created in `new`).
    device: Option<usize>,
    /// Per-touch-id bookkeeping: (id, x01, y01, active) so we can emit correct
    /// DOWN/MOVE/UP transitions between client messages.
    state: Mutex<Vec<(u32, f32, f32, bool)>>,
}

impl TouchInjector {
    /// Create the synthetic pointer device targeting `mapping.hmonitor`. Injected
    /// coordinates are then relative to that monitor (0,0 = its top-left), which is
    /// why `InjectSyntheticPointerInput` (unlike `InjectTouchInput`) works for
    /// monitors that sit left of / above the primary.
    pub fn new(mapping: &ScreenMapping) -> Self {
        let api = TouchApi::resolve();
        let device = match api.as_ref() {
            Some(api) => {
                let h = unsafe {
                    (api.create)(
                        PT_TOUCH,
                        10,
                        POINTER_FEEDBACK_NONE,
                        mapping.hmonitor,
                        0,
                        0,
                        SDCO_NONE,
                    )
                };
                if h == 0 {
                    let err = unsafe { GetLastError() };
                    println!("touch: CreateSyntheticPointerDevice failed, lastErr={err}");
                }
                (h != 0).then_some(h)
            }
            None => None,
        };
        Self {
            api,
            device,
            state: Mutex::new(Vec::new()),
        }
    }

    pub fn unsupported(&self) -> bool {
        self.api.is_none() || self.device.is_none()
    }

    /// Destroy the synthetic pointer device, releasing the pointer-insertion handle.
    pub fn shutdown(&mut self) {
        if let (Some(api), Some(device)) = (self.api.as_ref(), self.device.take()) {
            unsafe { (api.destroy)(device) };
        }
    }

    /// Build the `POINTER_TYPE_INFO` array for a decoded touch message and inject it
    /// through the synthetic pointer device. `events` is `&[(id, active, x01, y01)]`
    /// (see `protocol::decode_touch`). Coordinates are monitor-relative (0..width,
    /// 0..height) because the device was created bound to the captured monitor.
    pub fn apply_and_send(
        &mut self,
        events: &[(u8, bool, f32, f32)],
        mapping: &ScreenMapping,
    ) -> Result<(), TouchError> {
        let Some(api) = self.api.as_ref() else {
            return Err(TouchError::Unsupported);
        };
        let Some(device) = self.device else {
            return Err(TouchError::Unsupported);
        };
        if events.is_empty() {
            return Ok(());
        }

        let mut infos: Vec<PointerTypeInfo> = Vec::with_capacity(events.len());
        {
            let mut state = self.state.lock().unwrap();
            for (id, active, x01, y01) in events {
                let tid = *id as u32;
                // Monitor-relative coordinates: (0,0) is the captured monitor's top-left.
                // Treat network coordinates as untrusted input. Clamping here
                // prevents NaN/out-of-range values from becoming invalid Win32
                // pointer coordinates at the edge of a monitor.
                let nx = if x01.is_finite() { x01.clamp(0.0, 1.0) } else { 0.0 };
                let ny = if y01.is_finite() { y01.clamp(0.0, 1.0) } else { 0.0 };
                let x = (nx * mapping.width.saturating_sub(1) as f32).round() as i32;
                let y = (ny * mapping.height.saturating_sub(1) as f32).round() as i32;

                let prev = state.iter().find(|s| s.0 == tid);
                let (prev_active, prev_x, prev_y) = match prev {
                    Some(s) => (s.3, s.1, s.2),
                    None => (false, nx, ny),
                };

                let flags = if *active && !prev_active {
                    POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
                } else if *active && (prev_x != nx || prev_y != ny) {
                    POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
                } else if !*active && prev_active {
                    POINTER_FLAG_UP
                } else {
                    continue; // nothing changed
                };

                infos.push(PointerTypeInfo {
                    pointer_type: PT_TOUCH,
                    touch_info: PointerTouchInfo {
                        pointer_info: PointerInfo {
                            pointer_type: PT_TOUCH,
                            pointer_id: tid,
                            frame_id: 0,
                            pointer_flags: flags,
                            pt_pixel_location: Point { x, y },
                            ..PointerInfo::default()
                        },
                        touch_flags: TOUCH_FLAG_NONE,
                        touch_mask: TOUCH_MASK_CONTACTAREA,
                        rc_contact: Rect {
                            left: x - FINGER_RADIUS,
                            top: y - FINGER_RADIUS,
                            right: x + FINGER_RADIUS,
                            bottom: y + FINGER_RADIUS,
                        },
                        ..PointerTouchInfo::default()
                    },
                });

                // update bookkeeping
                if let Some(s) = state.iter_mut().find(|s| s.0 == tid) {
                    s.1 = nx;
                    s.2 = ny;
                    s.3 = *active;
                } else {
                    state.push((tid, nx, ny, *active));
                }
            }
        }

        if !infos.is_empty() {
            let ok = unsafe { (api.inject)(device, infos.as_ptr(), infos.len() as u32) };
            if ok == 0 {
                let err = unsafe { GetLastError() };
                eprintln!(
                    "touch inject error: SendFailed lastErr={err} size={}x{} ptr_type_info_size={} n={}",
                    mapping.width,
                    mapping.height,
                    std::mem::size_of::<PointerTypeInfo>(),
                    infos.len()
                );
                for ti in &infos {
                    let p = &ti.touch_info.pointer_info;
                    eprintln!(
                        "  contact id={} flags=0x{:08X} pt=({},{}) rc=({},{},{},{})",
                        p.pointer_id,
                        p.pointer_flags,
                        p.pt_pixel_location.x,
                        p.pt_pixel_location.y,
                        ti.touch_info.rc_contact.left,
                        ti.touch_info.rc_contact.top,
                        ti.touch_info.rc_contact.right,
                        ti.touch_info.rc_contact.bottom
                    );
                }
                return Err(TouchError::SendFailed);
            }
        }
        Ok(())
    }
}
