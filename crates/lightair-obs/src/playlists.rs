//! Media playlists (docs/13, "Built-in replacements"): a media object whose settings have a
//! `playlist` (files) plays them one after another (`shuffle`: in a random order, new each time
//! round; `looping`: from the top again after the last). A libobs tick looks for the file ending
//! and starts the next. Where a playlist is is runtime state, never in the document.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use libobs_sys as sys;
use lightair_model::Id;
use parking_lot::Mutex;
use serde_json::{Value, json};

/// The files of a media source's playlist (empty: it has none).
pub fn files(settings: &Value) -> Vec<String> {
    settings["playlist"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

/// The media source's settings for a playlist (its first file; never looping one file).
pub fn media_settings(settings: &Value) -> Value {
    let mut s = settings.clone();
    if let Some(first) = files(settings).first() {
        s["local_file"] = json!(first);
        s["is_local_file"] = json!(true);
        s["looping"] = json!(false);
        s["restart_on_activate"] = json!(false);
        s["close_when_inactive"] = json!(false);
    }
    s
}

struct Entry {
    src: usize,
    files: Vec<String>,
    /// The play order (indices into `files`).
    order: Vec<usize>,
    at: usize,
    looping: bool,
    shuffle: bool,
    /// When the current file was started (a new file reports Ended for a moment).
    started: Instant,
}

impl Entry {
    fn shuffled(n: usize, shuffle: bool) -> Vec<usize> {
        let mut order: Vec<usize> = (0..n).collect();
        if shuffle {
            let mut seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64) | 1;
            for i in (1..n).rev() {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                order.swap(i, (seed % (i as u64 + 1)) as usize);
            }
        }
        order
    }

    fn file(&self) -> Option<&String> {
        self.order.get(self.at).and_then(|i| self.files.get(*i))
    }

    unsafe fn play(&mut self) {
        let Some(file) = self.file().cloned() else { return };
        let src = self.src as *mut sys::obs_source_t;
        let data = crate::obs::Data::from_json(&json!({"local_file": file, "is_local_file": true, "looping": false}));
        unsafe {
            sys::obs_source_update(src, data.as_ptr());
            sys::obs_source_media_restart(src);
        }
        self.started = Instant::now();
    }
}

#[derive(Default)]
pub struct Playlists {
    inner: Mutex<HashMap<Id, Entry>>,
}

impl Playlists {
    /// Follows `settings` for `source` (a playlist, or none any more).
    pub fn set(&self, source: Id, src: *mut sys::obs_source_t, settings: &Value) {
        let files = files(settings);
        let mut inner = self.inner.lock();
        if files.is_empty() {
            inner.remove(&source);
            return;
        }
        let looping = settings["looping"].as_bool().unwrap_or(true);
        let shuffle = settings["shuffle"].as_bool().unwrap_or(false);
        match inner.get_mut(&source) {
            // the same files: keep its place
            Some(e) if e.files == files => {
                e.looping = looping;
                if e.shuffle != shuffle {
                    e.shuffle = shuffle;
                    e.order = Entry::shuffled(files.len(), shuffle);
                    e.at = 0;
                }
                e.src = src as usize;
            }
            _ => {
                let n = files.len();
                inner.insert(
                    source,
                    Entry { src: src as usize, files, order: Entry::shuffled(n, shuffle), at: 0, looping, shuffle, started: Instant::now() },
                );
            }
        }
    }

    pub fn remove(&self, source: Id) {
        self.inner.lock().remove(&source);
    }

    /// Moves `by` files (wrapping) and plays from there.
    pub fn skip(&self, source: Id, by: i64) {
        let mut inner = self.inner.lock();
        let Some(e) = inner.get_mut(&source) else { return };
        let n = e.order.len() as i64;
        e.at = (((e.at as i64 + by) % n + n) % n) as usize;
        unsafe { e.play() };
    }

    /// (position from 1, count, the file playing).
    pub fn now(&self, source: Id) -> Option<(usize, usize, String)> {
        let inner = self.inner.lock();
        let e = inner.get(&source)?;
        Some((e.at + 1, e.order.len(), e.file()?.clone()))
    }

    fn tick(&self) {
        let Some(mut inner) = self.inner.try_lock() else { return };
        for e in inner.values_mut() {
            if e.started.elapsed().as_millis() < 800 {
                continue;
            }
            let state = unsafe { sys::obs_source_media_get_state(e.src as *mut sys::obs_source_t) };
            if state != sys::OBS_MEDIA_STATE_ENDED {
                continue;
            }
            if e.at + 1 < e.order.len() {
                e.at += 1;
            } else if e.looping {
                e.at = 0;
                e.order = Entry::shuffled(e.files.len(), e.shuffle);
            } else {
                continue;
            }
            unsafe { e.play() };
        }
    }
}

pub(crate) unsafe extern "C" fn on_tick(param: *mut std::ffi::c_void, _seconds: f32) {
    let p = unsafe { &*(param as *const Playlists) };
    p.tick();
}

pub(crate) fn tick_param(p: &Arc<Playlists>) -> *mut std::ffi::c_void {
    Arc::as_ptr(p) as *mut _
}
