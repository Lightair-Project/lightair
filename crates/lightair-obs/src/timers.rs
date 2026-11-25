//! Timers' libobs side (the timers are `lightair_host::timers`, re-exported here): a timer's
//! libobs text source, rewritten with its text, and libobs's tick driving the timers.

use std::sync::Arc;

use lightair_model::Id;
use serde_json::Value;

pub use lightair_host::timers::*;

use crate::obs::Data;
use crate::sys;

/// The libobs source that draws a timer.
pub const TIMER_SOURCE: &str = "text_ft2_source_v2";

/// A timer's libobs text source (a strong reference).
struct TextSource(*mut sys::obs_source_t);

// SAFETY: a strong libobs reference; libobs sources are thread-safe to update and query.
unsafe impl Send for TextSource {}

impl TimerOut for TextSource {
    fn text(&self, text: &str) {
        let mut d = Data::new();
        d.set_str("text", text);
        unsafe { sys::obs_source_update(self.0, d.as_ptr()) };
    }

    fn on_air(&self) -> bool {
        unsafe { sys::obs_source_active(self.0) }
    }
}

impl Drop for TextSource {
    fn drop(&mut self) {
        unsafe { sys::obs_source_release(self.0) };
    }
}

/// Adds the timer of document source `id`, shown by libobs text source `source`.
pub(crate) fn add(timers: &Timers, id: Id, source: *mut sys::obs_source_t, settings: &Value) {
    let source = unsafe { sys::obs_source_get_ref(source) };
    if source.is_null() {
        return;
    }
    timers.add(id, settings, Some(Box::new(TextSource(source))));
}

pub(crate) unsafe extern "C" fn on_timers_tick(param: *mut std::ffi::c_void, _seconds: f32) {
    let timers = unsafe { &*(param as *const Timers) };
    timers.tick();
}

pub(crate) fn tick_param(timers: &Arc<Timers>) -> *mut std::ffi::c_void {
    Arc::as_ptr(timers) as *mut _
}
