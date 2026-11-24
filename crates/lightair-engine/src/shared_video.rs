//! Pictures other apps share on this computer (docs/13, "Built-in replacements"): Syphon on a
//! Mac (OBS's `syphon-input`, from the mac-syphon module Lightair ships), Spout on Windows
//! (Lightair's own receiver: Spout's sender list and texture info in shared memory, the texture
//! opened through Direct3D 11's sharing).

use serde_json::{Value, json};

/// The source kind that shows a shared picture here.
#[cfg(not(windows))]
pub const KIND: &str = "syphon-input";
#[cfg(windows)]
pub const KIND: &str = "lightair_spout";

/// The pictures being shared now: (what to call it, its id for [`settings`]). Call on the engine
/// thread.
#[cfg(not(windows))]
pub fn senders() -> Vec<(String, String)> {
    crate::obs::property_items(KIND, "uuid").into_iter().filter(|(_, id)| !id.is_empty()).collect()
}

/// A source's settings for the picture `id` (`name`: what [`senders`] called it).
#[cfg(not(windows))]
pub fn settings(id: &str, _name: &str) -> Value {
    json!({ "uuid": id, "allow_transparency": true })
}

#[cfg(windows)]
pub fn senders() -> Vec<(String, String)> {
    spout::names().into_iter().map(|n| (n.clone(), n)).collect()
}

#[cfg(windows)]
pub fn settings(id: &str, _name: &str) -> Value {
    json!({ "sender": id })
}

/// Registers Lightair's own shared-picture source where it has one (Windows).
pub fn register() {
    #[cfg(windows)]
    spout::register();
}

#[cfg(windows)]
mod spout {
    //! Spout 2's conventions: the shared memory "SpoutSenderNames" lists senders (256 bytes a
    //! name, an empty one ending the list); a sender's own shared memory, named after it, starts
    //! with its texture's share handle (32 bits), width, height, DXGI format and usage.

    use std::ffi::{CStr, CString, c_char, c_void};

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Memory::{FILE_MAP_READ, MapViewOfFile, OpenFileMappingA, UnmapViewOfFile};

    use crate::sys;

    unsafe extern "C" {
        /// libobs (Direct3D 11): a texture another process shares, by its handle.
        fn gs_texture_open_shared(handle: u32) -> *mut sys::gs_texture_t;
    }

    const NAME_LEN: usize = 256;
    const MAX_SENDERS: usize = 64;

    /// Reads `len` bytes of the shared memory `name`, if it exists.
    fn read_shared(name: &str, len: usize) -> Option<Vec<u8>> {
        let n = CString::new(name).ok()?;
        unsafe {
            let h = OpenFileMappingA(FILE_MAP_READ, 0, n.as_ptr() as *const u8);
            if h.is_null() {
                return None;
            }
            let view = MapViewOfFile(h, FILE_MAP_READ, 0, 0, len);
            let out = if view.Value.is_null() {
                None
            } else {
                let bytes = std::slice::from_raw_parts(view.Value as *const u8, len).to_vec();
                UnmapViewOfFile(view);
                Some(bytes)
            };
            CloseHandle(h);
            out
        }
    }

    /// The senders' names, as Spout lists them.
    pub fn names() -> Vec<String> {
        // the list is at most this long; a shorter mapping reads what there is
        let Some(bytes) = (1..=MAX_SENDERS).rev().find_map(|n| read_shared("SpoutSenderNames", n * NAME_LEN)) else {
            return Vec::new();
        };
        bytes
            .chunks(NAME_LEN)
            .map(|c| {
                let end = c.iter().position(|b| *b == 0).unwrap_or(c.len());
                String::from_utf8_lossy(&c[..end]).into_owned()
            })
            .take_while(|n| !n.is_empty())
            .collect()
    }

    /// A sender's texture: (share handle, width, height).
    fn info(sender: &str) -> Option<(u32, u32, u32)> {
        let b = read_shared(sender, 20)?;
        let u = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let (handle, w, h) = (u(0), u(4), u(8));
        (handle != 0 && w > 0 && h > 0).then_some((handle, w, h))
    }

    struct Spout {
        sender: String,
        texture: *mut sys::gs_texture_t,
        /// The handle and size the texture was opened with.
        opened: (u32, u32, u32),
        size: (u32, u32),
    }

    unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
        c"Spout".as_ptr()
    }

    unsafe extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
        let s = unsafe { &mut *(data as *mut Spout) };
        let p = unsafe { sys::obs_data_get_string(settings, c"sender".as_ptr()) };
        s.sender = if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() };
    }

    unsafe extern "C" fn create(settings: *mut sys::obs_data_t, _: *mut sys::obs_source_t) -> *mut c_void {
        let mut s = Box::new(Spout { sender: String::new(), texture: std::ptr::null_mut(), opened: (0, 0, 0), size: (0, 0) });
        unsafe { update(&mut *s as *mut Spout as *mut c_void, settings) };
        Box::into_raw(s) as *mut c_void
    }

    unsafe extern "C" fn destroy(data: *mut c_void) {
        let s = unsafe { Box::from_raw(data as *mut Spout) };
        if !s.texture.is_null() {
            unsafe {
                sys::obs_enter_graphics();
                sys::gs_texture_destroy(s.texture);
                sys::obs_leave_graphics();
            }
        }
    }

    unsafe extern "C" fn width(data: *mut c_void) -> u32 {
        unsafe { &*(data as *const Spout) }.size.0
    }

    unsafe extern "C" fn height(data: *mut c_void) -> u32 {
        unsafe { &*(data as *const Spout) }.size.1
    }

    unsafe extern "C" fn render(data: *mut c_void, _: *mut sys::gs_effect_t) {
        let s = unsafe { &mut *(data as *mut Spout) };
        let now = info(&s.sender);
        unsafe {
            if now != Some(s.opened) {
                if !s.texture.is_null() {
                    sys::gs_texture_destroy(s.texture);
                    s.texture = std::ptr::null_mut();
                }
                if let Some((handle, w, h)) = now {
                    s.texture = gs_texture_open_shared(handle);
                    s.opened = (handle, w, h);
                    s.size = (w, h);
                } else {
                    s.opened = (0, 0, 0);
                }
            }
            if s.texture.is_null() {
                return;
            }
            let effect = sys::obs_get_base_effect(sys::OBS_EFFECT_DEFAULT);
            sys::gs_effect_set_texture(sys::gs_effect_get_param_by_name(effect, c"image".as_ptr()), s.texture);
            while sys::gs_effect_loop(effect, c"Draw".as_ptr()) {
                sys::gs_draw_sprite(s.texture, 0, s.size.0, s.size.1);
            }
        }
    }

    pub fn register() {
        static DONE: std::sync::Once = std::sync::Once::new();
        DONE.call_once(|| unsafe {
            let mut info: sys::obs_source_info = std::mem::zeroed();
            info.id = c"lightair_spout".as_ptr();
            info.type_ = sys::OBS_SOURCE_TYPE_INPUT;
            info.output_flags = sys::OBS_SOURCE_VIDEO | sys::OBS_SOURCE_CUSTOM_DRAW;
            info.get_name = Some(get_name);
            info.create = Some(create);
            info.destroy = Some(destroy);
            info.update = Some(update);
            info.get_width = Some(width);
            info.get_height = Some(height);
            info.video_render = Some(render);
            sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
        });
    }
}
