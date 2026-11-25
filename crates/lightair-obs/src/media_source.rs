//! The media source's libobs side (the player is `lightair_host::media_player`, re-exported
//! here): `lightair_media`, a shell over a [`MediaPlayer`] that draws its frames where libobs
//! shows the source (a texture per surface, turned by the file's rotation), hands libobs the
//! sound no track takes, and tells libobs when the file ends or starts over. [`crate::kinds`]
//! routes the files AVFoundation plays (local MP4 and QuickTime files) here.

use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_void};
use std::sync::Arc;

use parking_lot::Mutex;

use lightair_host::media_player::{Elsewhere, MediaPlayer, Next, Settings, Slot};
pub use lightair_host::media_player::{Frames, Picture, picture, plays};

use crate::sys;

/// The libobs source type.
pub const ID: &CStr = c"lightair_media";

/// Frames libobs may still be drawing from: kept for a few frames after replaced.
const HELD: usize = 3;

// the media states libobs hands on are the player's codes as they are
const _: () = assert!(sys::OBS_MEDIA_STATE_PLAYING == lightair_host::media_player::state::PLAYING);
const _: () = assert!(sys::OBS_MEDIA_STATE_ERROR == lightair_host::media_player::state::ERROR);

struct Texture(*mut sys::gs_texture_t);
// SAFETY: only touched on libobs's graphics thread.
unsafe impl Send for Texture {}

#[derive(Default)]
struct Shown {
    /// The slot on screen, then the ones before it libobs may still be drawing.
    held: VecDeque<Arc<Slot>>,
    /// A texture per slot, made the first time it shows and kept while the slot lives (making
    /// one costs about as much as the rest of a frame's rendering).
    textures: Vec<(u64, std::sync::Weak<Slot>, Texture)>,
    showing: bool,
    size: (u32, u32),
}

impl Shown {
    /// Shows `slot`. Graphics thread.
    unsafe fn show(&mut self, slot: Arc<Slot>) {
        // textures of slots gone (a new file's size)
        self.textures.retain(|(_, s, Texture(t))| {
            let live = s.strong_count() > 0;
            if !live {
                unsafe { sys::gs_texture_destroy(*t) };
            }
            live
        });
        if !self.textures.iter().any(|(id, ..)| *id == slot.id) {
            let t = unsafe { sys::gs_texture_create_from_iosurface(slot.surface()) };
            if t.is_null() {
                self.showing = false;
                return;
            }
            self.textures.push((slot.id, Arc::downgrade(&slot), Texture(t)));
        }
        self.size = slot.size;
        self.showing = true;
        self.held.push_front(slot);
        self.held.truncate(HELD);
    }

    /// The texture on screen.
    fn texture(&self) -> Option<*mut sys::gs_texture_t> {
        let id = self.held.front().filter(|_| self.showing)?.id;
        self.textures.iter().find(|(i, ..)| *i == id).map(|(.., Texture(t))| *t)
    }

    /// Shows nothing. Graphics thread.
    fn clear(&mut self) {
        self.held.clear();
        self.showing = false;
    }

    /// Lets go of every texture. Graphics thread.
    unsafe fn destroy(&mut self) {
        self.held.clear();
        for (.., Texture(t)) in self.textures.drain(..) {
            unsafe { sys::gs_texture_destroy(t) };
        }
    }
}

/// Where the player tells libobs what happens.
struct ToLibobs(usize);

impl ToLibobs {
    fn source(&self) -> *mut sys::obs_source_t {
        self.0 as *mut sys::obs_source_t
    }
}

impl Elsewhere for ToLibobs {
    fn audio(&self, interleaved: &[f32], rate: u32, timestamp_ns: u64) {
        let null = std::ptr::null::<u8>();
        let audio = sys::obs_source_audio {
            data: [interleaved.as_ptr() as *const u8, null, null, null, null, null, null, null],
            frames: (interleaved.len() / 2) as u32,
            speakers: sys::SPEAKERS_STEREO,
            format: sys::AUDIO_FORMAT_FLOAT,
            samples_per_sec: rate,
            timestamp: timestamp_ns,
        };
        unsafe { sys::obs_source_output_audio(self.source(), &audio) };
    }

    fn ended(&self) {
        unsafe { sys::obs_source_media_ended(self.source()) };
    }

    fn started(&self) {
        unsafe { sys::obs_source_media_started(self.source()) };
    }
}

struct Media {
    player: MediaPlayer,
    shown: Mutex<Shown>,
    source: usize,
}

fn media<'a>(data: *mut c_void) -> &'a Media {
    unsafe { &*(data as *const Media) }
}

fn settings_of(settings: *mut sys::obs_data_t) -> Settings {
    unsafe {
        let path = sys::obs_data_get_string(settings, c"local_file".as_ptr());
        Settings {
            path: if path.is_null() { String::new() } else { CStr::from_ptr(path).to_string_lossy().into_owned() },
            looping: sys::obs_data_get_bool(settings, c"looping".as_ptr()),
            restart_on_activate: sys::obs_data_get_bool(settings, c"restart_on_activate".as_ptr()),
            clear_on_end: sys::obs_data_get_bool(settings, c"clear_on_media_end".as_ptr()),
        }
    }
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"Media".as_ptr()
}

unsafe extern "C" fn get_defaults(settings: *mut sys::obs_data_t) {
    unsafe {
        sys::obs_data_set_default_bool(settings, c"is_local_file".as_ptr(), true);
        sys::obs_data_set_default_bool(settings, c"looping".as_ptr(), false);
        sys::obs_data_set_default_bool(settings, c"restart_on_activate".as_ptr(), true);
        sys::obs_data_set_default_bool(settings, c"clear_on_media_end".as_ptr(), true);
    }
}

unsafe extern "C" fn create(settings: *mut sys::obs_data_t, source: *mut sys::obs_source_t) -> *mut c_void {
    let mut info = sys::obs_audio_info::default();
    let rate = if unsafe { sys::obs_get_audio_info(&mut info) } && info.samples_per_sec > 0 {
        info.samples_per_sec
    } else {
        48000
    };
    let private = unsafe { sys::obs_obj_is_private(source as *mut c_void) };
    // a document's source (named by its id) is found by the compositor and fed to the mixer
    let id = if private { None } else { crate::obs::doc_id(source) };
    // plays once on air (from the start); a private one (the Library's player) at once
    let play_now = unsafe { sys::obs_source_active(source) } || private;
    let elsewhere: Arc<dyn Elsewhere> = Arc::new(ToLibobs(source as usize));
    let player = MediaPlayer::new(id, settings_of(settings), rate, play_now, Some(elsewhere));
    Box::into_raw(Box::new(Media { player, shown: Mutex::new(Shown::default()), source: source as usize }))
        as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let m = unsafe { Box::from_raw(data as *mut Media) };
    let Media { player, shown, .. } = *m;
    // the reader stops first
    drop(player);
    let mut shown = shown.lock();
    unsafe {
        sys::obs_enter_graphics();
        shown.destroy();
        sys::obs_leave_graphics();
    }
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let m = media(data);
    let on_air = unsafe { sys::obs_source_active(m.source as *mut sys::obs_source_t) };
    m.player.update(settings_of(settings), on_air);
}

unsafe extern "C" fn activate(data: *mut c_void) {
    let m = media(data);
    if m.player.activate() {
        unsafe { sys::obs_source_media_started(m.source as *mut sys::obs_source_t) };
    }
}

unsafe extern "C" fn deactivate(data: *mut c_void) {
    media(data).player.deactivate();
}

/// Picks the frame due now (libobs's graphics thread, before rendering).
unsafe extern "C" fn video_tick(data: *mut c_void, _seconds: f32) {
    media(data).player.tick();
}

unsafe extern "C" fn video_render(data: *mut c_void, _effect: *mut sys::gs_effect_t) {
    let m = media(data);
    let mut shown = m.shown.lock();
    match m.player.take_next() {
        Some(Next::Show(p)) => unsafe { shown.show(p) },
        Some(Next::Clear) => shown.clear(),
        None => {}
    }
    let Some(tex) = shown.texture() else { return };
    let (w, h) = shown.size;
    let rotation = m.player.natural().1;
    unsafe {
        // an IOSurface is a rectangle texture on OpenGL (Metal has none)
        let rect = sys::gs_get_device_type() as u32 != sys::GS_DEVICE_METAL;
        let effect =
            sys::obs_get_base_effect(if rect { sys::OBS_EFFECT_DEFAULT_RECT } else { sys::OBS_EFFECT_DEFAULT });
        let image = sys::gs_effect_get_param_by_name(effect, c"image".as_ptr());
        sys::gs_effect_set_texture(image, tex);
        sys::gs_matrix_push();
        // the file's rotation (a phone held upright): turned as its players show it
        let (fw, fh) = (w as f32, h as f32);
        match rotation {
            90 => {
                sys::gs_matrix_translate3f(fh, 0.0, 0.0);
                sys::gs_matrix_rotaa4f(0.0, 0.0, 1.0, std::f32::consts::FRAC_PI_2);
            }
            180 => {
                sys::gs_matrix_translate3f(fw, fh, 0.0);
                sys::gs_matrix_rotaa4f(0.0, 0.0, 1.0, std::f32::consts::PI);
            }
            270 => {
                sys::gs_matrix_translate3f(0.0, fw, 0.0);
                sys::gs_matrix_rotaa4f(0.0, 0.0, 1.0, -std::f32::consts::FRAC_PI_2);
            }
            _ => {}
        }
        while sys::gs_effect_loop(effect, c"Draw".as_ptr()) {
            sys::gs_draw_sprite(tex, 0, w, h);
        }
        sys::gs_matrix_pop();
    }
}

fn size(data: *mut c_void) -> (u32, u32) {
    let m = media(data);
    let shown = m.shown.lock().size;
    let (natural, rotation) = m.player.natural();
    let (w, h) = if shown.0 > 0 { shown } else { natural };
    if rotation % 180 == 90 { (h, w) } else { (w, h) }
}

unsafe extern "C" fn get_width(data: *mut c_void) -> u32 {
    size(data).0
}

unsafe extern "C" fn get_height(data: *mut c_void) -> u32 {
    size(data).1
}

unsafe extern "C" fn media_play_pause(data: *mut c_void, pause: bool) {
    media(data).player.play_pause(pause);
}

unsafe extern "C" fn media_restart(data: *mut c_void) {
    let m = media(data);
    m.player.restart();
    unsafe { sys::obs_source_media_started(m.source as *mut sys::obs_source_t) };
}

unsafe extern "C" fn media_stop(data: *mut c_void) {
    media(data).player.stop();
}

unsafe extern "C" fn media_get_duration(data: *mut c_void) -> i64 {
    media(data).player.duration_ms()
}

unsafe extern "C" fn media_get_time(data: *mut c_void) -> i64 {
    media(data).player.time_ms()
}

unsafe extern "C" fn media_set_time(data: *mut c_void, ms: i64) {
    media(data).player.set_time_ms(ms);
}

unsafe extern "C" fn media_get_state(data: *mut c_void) -> sys::obs_media_state {
    media(data).player.state()
}

/// Registers the source with libobs (once; after startup).
pub fn register() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = ID.as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        info.output_flags = sys::OBS_SOURCE_VIDEO
            | sys::OBS_SOURCE_AUDIO
            | sys::OBS_SOURCE_CUSTOM_DRAW
            | sys::OBS_SOURCE_DO_NOT_DUPLICATE
            | sys::OBS_SOURCE_CONTROLLABLE_MEDIA;
        info.get_name = Some(get_name);
        info.create = Some(create);
        info.destroy = Some(destroy);
        info.update = Some(update);
        info.get_defaults = Some(get_defaults);
        info.activate = Some(activate);
        info.deactivate = Some(deactivate);
        info.video_tick = Some(video_tick);
        info.video_render = Some(video_render);
        info.get_width = Some(get_width);
        info.get_height = Some(get_height);
        info.media_play_pause = Some(media_play_pause);
        info.media_restart = Some(media_restart);
        info.media_stop = Some(media_stop);
        info.media_get_duration = Some(media_get_duration);
        info.media_get_time = Some(media_get_time);
        info.media_set_time = Some(media_set_time);
        info.media_get_state = Some(media_get_state);
        info.icon_type = sys::OBS_ICON_TYPE_MEDIA;
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}
