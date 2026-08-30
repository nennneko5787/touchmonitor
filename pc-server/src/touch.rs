//! Multi-touch injection into Windows via the user32 `InitializeTouchInjection` /
//! `SendTouchInput` API. This is the only OS-level way to deliver true multi-touch
//! (as opposed to a single synthetic mouse pointer).
//!
//! The `windows` crate does not project these two functions, and the MSVC import
//! library does not reliably expose their import thunks, so we resolve them at
//! runtime with `GetProcAddress`. user32.dll is always mapped, so this always
//! succeeds. The `TOUCHINPUT` struct matches the documented C layout.

use std::sync::Mutex;

/// The Windows `TOUCHINPUT` structure (user32 / winuser.h). `#[repr(C)]` layout
/// must match the OS definition exactly.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TouchInput {
    pub x: i32,
    pub y: i32,
    pub h_source: usize,
    pub dw_id: u32,
    pub dw_flags: u32,
    pub dw_mask: u32,
    pub dw_time: u32,
    pub dw_extra_info: usize,
    pub cx_contact: u32,
    pub cy_contact: u32,
}

type InitTouchFn = unsafe extern "system" fn(maximum_touches: u32, feedback_mode: u32) -> i32;
type SendTouchFn = unsafe extern "system" fn(count: u32, inputs: *const TouchInput) -> i32;

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(lp_module_name: *const u16) -> *mut core::ffi::c_void;
    fn GetProcAddress(h_module: *mut core::ffi::c_void, lp_proc_name: *const u8) -> *mut core::ffi::c_void;
}

const TOUCHEVENTF_MOVE: u32 = 0x0001;
const TOUCHEVENTF_DOWN: u32 = 0x0002;
const TOUCHEVENTF_UP: u32 = 0x0004;
const TOUCHEVENTF_NOCOALESCE: u32 = 0x0020;

const TOUCHINPUTMASKF_TOUCHEVENTMASK: u32 = 0x0001;
const TOUCHINPUTMASKF_CONTACTAREA: u32 = 0x0004;

const TOUCH_FEEDBACK_DEFAULT: u32 = 0x1;

/// Dynamically-resolved user32 touch injection entry points.
struct TouchApi {
    init: InitTouchFn,
    send: SendTouchFn,
}

impl TouchApi {
    fn resolve() -> Option<Self> {
        let name: Vec<u16> = "user32\0".encode_utf16().collect();
        let user32 = unsafe { GetModuleHandleW(name.as_ptr()) };
        if user32.is_null() {
            return None;
        }
        let init_ptr = unsafe { GetProcAddress(user32, b"InitializeTouchInjection\0".as_ptr()) };
        let send_ptr = unsafe { GetProcAddress(user32, b"SendTouchInput\0".as_ptr()) };
        if init_ptr.is_null() || send_ptr.is_null() {
            return None;
        }
        let init = unsafe { std::mem::transmute::<*mut core::ffi::c_void, InitTouchFn>(init_ptr) };
        let send = unsafe { std::mem::transmute::<*mut core::ffi::c_void, SendTouchFn>(send_ptr) };
        Some(Self { init, send })
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

pub enum TouchError {
    Unsupported,
    SendFailed,
}

pub struct TouchInjector {
    api: Option<TouchApi>,
    /// Per-touch-id bookkeeping: (id, x01, y01, active) so we can emit correct
    /// DOWN/MOVE/UP transitions between client messages.
    state: Mutex<Vec<(u32, f32, f32, bool)>>,
}

impl TouchInjector {
    pub fn new() -> Self {
        let api = TouchApi::resolve();
        if let Some(ref api) = api {
            // Prepare the OS for up to 10 simultaneous touch points.
            unsafe { (api.init)(10, TOUCH_FEEDBACK_DEFAULT) };
        }
        Self {
            api,
            state: Mutex::new(Vec::new()),
        }
    }

    pub fn unsupported(&self) -> bool {
        self.api.is_none()
    }

    /// Build the user32 `TOUCHINPUT` array for a decoded touch message and send it.
    /// `events` is `&[(id, active, x01, y01)]` (see `protocol::decode_touch`).
    pub fn apply_and_send(
        &mut self,
        events: &[(u8, bool, f32, f32)],
        mapping: &ScreenMapping,
    ) -> Result<(), TouchError> {
        let Some(ref api) = self.api else {
            return Err(TouchError::Unsupported);
        };
        if events.is_empty() {
            return Ok(());
        }

        let mut inputs: Vec<TouchInput> = Vec::with_capacity(events.len());
        {
            let mut state = self.state.lock().unwrap();
            for (id, active, x01, y01) in events {
                let tid = *id as u32;
                let x = mapping.origin_x + (*x01 * mapping.width as f32) as i32;
                let y = mapping.origin_y + (*y01 * mapping.height as f32) as i32;

                let prev = state.iter().find(|s| s.0 == tid);
                let (prev_active, prev_x, prev_y) = match prev {
                    Some(s) => (s.3, s.1, s.2),
                    None => (false, *x01, *y01),
                };

                let flag = if *active && !prev_active {
                    TOUCHEVENTF_DOWN
                } else if *active && (prev_x != *x01 || prev_y != *y01) {
                    TOUCHEVENTF_MOVE
                } else if !*active && prev_active {
                    TOUCHEVENTF_UP
                } else {
                    continue; // nothing changed
                };

                inputs.push(TouchInput {
                    x,
                    y,
                    h_source: 0,
                    dw_id: tid,
                    dw_flags: flag | TOUCHEVENTF_NOCOALESCE,
                    dw_mask: TOUCHINPUTMASKF_TOUCHEVENTMASK | TOUCHINPUTMASKF_CONTACTAREA,
                    dw_time: 0,
                    dw_extra_info: 0,
                    cx_contact: 10,
                    cy_contact: 10,
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

        if !inputs.is_empty() {
            let ok = unsafe { (api.send)(inputs.len() as u32, inputs.as_ptr()) };
            if ok == 0 {
                return Err(TouchError::SendFailed);
            }
        }
        Ok(())
    }
}
