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

type InitTouchFn = unsafe extern "system" fn(maximum_touches: u32, feedback_mode: u32) -> i32;
type InjectTouchFn = unsafe extern "system" fn(count: u32, contacts: *const PointerTouchInfo) -> i32;

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

// POINTER_INPUT_TYPE: PT_TOUCH
const PT_TOUCH: u32 = 0x0000_0002;

// TOUCH_MASK_* (winuser.h)
const TOUCH_MASK_CONTACTAREA: u32 = 0x0000_0001;

// TOUCH_FLAG_NONE
const TOUCH_FLAG_NONE: u32 = 0x0000_0000;

const TOUCH_FEEDBACK_DEFAULT: u32 = 0x1;

/// Contact radius for an injected finger, in pixels.
const FINGER_RADIUS: i32 = 8;

/// Dynamically-resolved user32 touch injection entry points.
struct TouchApi {
    init: InitTouchFn,
    inject: InjectTouchFn,
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
        let init_ptr = unsafe { GetProcAddress(user32, b"InitializeTouchInjection\0".as_ptr()) };
        let inject_ptr = unsafe { GetProcAddress(user32, b"InjectTouchInput\0".as_ptr()) };
        if init_ptr.is_null() {
            println!("touch: InitializeTouchInjection not found");
            return None;
        }
        if inject_ptr.is_null() {
            println!("touch: InjectTouchInput not found");
            return None;
        }
        let init = unsafe { std::mem::transmute::<*mut core::ffi::c_void, InitTouchFn>(init_ptr) };
        let inject = unsafe { std::mem::transmute::<*mut core::ffi::c_void, InjectTouchFn>(inject_ptr) };
        Some(Self { init, inject })
    }
}

/// Maps normalized (0..1) stream coordinates back into screen pixel space for the
/// monitor being captured.
#[derive(Clone, Copy)]
pub struct ScreenMapping {
    pub origin_x: i32,
    pub origin_y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug)]
pub enum TouchError {
    Unsupported,
    SendFailed,
}

pub struct TouchInjector {
    api: Option<TouchApi>,
    /// Set once `InitializeTouchInjection` has been called (lazily, on the same
    /// thread that performs the injection, per the Win32 requirement that init and
    /// inject happen on the same thread).
    initialized: bool,
    /// Per-touch-id bookkeeping: (id, x01, y01, active) so we can emit correct
    /// DOWN/MOVE/UP transitions between client messages.
    state: Mutex<Vec<(u32, f32, f32, bool)>>,
}

impl TouchInjector {
    pub fn new() -> Self {
        Self {
            api: TouchApi::resolve(),
            initialized: false,
            state: Mutex::new(Vec::new()),
        }
    }

    pub fn unsupported(&self) -> bool {
        self.api.is_none()
    }

    /// Ensure `InitializeTouchInjection` has run on the *current* thread before the
    /// first injection. Called from `apply_and_send`, guaranteeing same-thread init+inject.
    fn ensure_initialized(&mut self) {
        if self.initialized {
            return;
        }
        if let Some(api) = self.api.as_ref() {
            let ok = unsafe { (api.init)(10, TOUCH_FEEDBACK_DEFAULT) };
            if ok == 0 {
                let err = unsafe { GetLastError() };
                println!("touch: InitializeTouchInjection failed, lastErr={err}");
            } else {
                println!("touch: InitializeTouchInjection OK");
            }
        }
        self.initialized = true;
    }

    /// Build the user32 `POINTER_TOUCH_INFO` array for a decoded touch message and
    /// inject it. `events` is `&[(id, active, x01, y01)]` (see `protocol::decode_touch`).
    pub fn apply_and_send(
        &mut self,
        events: &[(u8, bool, f32, f32)],
        mapping: &ScreenMapping,
    ) -> Result<(), TouchError> {
        if self.api.is_none() {
            return Err(TouchError::Unsupported);
        }
        if events.is_empty() {
            return Ok(());
        }
        self.ensure_initialized();

        let mut contacts: Vec<PointerTouchInfo> = Vec::with_capacity(events.len());
        let api = self.api.as_ref().unwrap();
        {
            let mut state = self.state.lock().unwrap();
            for (id, active, x01, y01) in events {
                let tid = *id as u32;
                let x = (mapping.origin_x as f32 + *x01 * mapping.width as f32) as i32;
                let y = (mapping.origin_y as f32 + *y01 * mapping.height as f32) as i32;

                let prev = state.iter().find(|s| s.0 == tid);
                let (prev_active, prev_x, prev_y) = match prev {
                    Some(s) => (s.3, s.1, s.2),
                    None => (false, *x01, *y01),
                };

                let flags = if *active && !prev_active {
                    POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
                } else if *active && (prev_x != *x01 || prev_y != *y01) {
                    POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
                } else if !*active && prev_active {
                    POINTER_FLAG_UP
                } else {
                    continue; // nothing changed
                };

                contacts.push(PointerTouchInfo {
                    pointer_info: PointerInfo {
                        pointer_type: PT_TOUCH,
                        pointer_id: tid,
                        frame_id: 0,
                        pointer_flags: flags,
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
                });

                // update bookkeeping
                if let Some(s) = state.iter_mut().find(|s| s.0 == tid) {
                    s.1 = *x01;
                    s.2 = *y01;
                    s.3 = *active;
                } else {
                    state.push((tid, *x01, *y01, *active));
                }
            }
        }

        if !contacts.is_empty() {
            let ok = unsafe { (api.inject)(contacts.len() as u32, contacts.as_ptr()) };
            if ok == 0 {
                let err = unsafe { GetLastError() };
                eprintln!("touch inject error: SendFailed lastErr={err}");
                return Err(TouchError::SendFailed);
            }
        }
        Ok(())
    }
}
