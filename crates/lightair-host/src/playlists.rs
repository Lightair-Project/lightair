//! Media sources with a playlist (`playlist`: files played in turn): their files, and the
//! settings a player starts from.

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

/// A playlist's order and place: its files, the order they play in (shuffled anew each time
/// round with `shuffle`), which one plays, and whether it goes round again (`looping`).
#[derive(Debug, Clone)]
pub struct Order {
    files: Vec<String>,
    order: Vec<usize>,
    at: usize,
    looping: bool,
    shuffle: bool,
}

impl Order {
    /// The order a media source's settings give; `None` without a playlist.
    pub fn of(settings: &Value) -> Option<Order> {
        let files = files(settings);
        if files.is_empty() {
            return None;
        }
        let shuffle = settings["shuffle"].as_bool().unwrap_or(false);
        let n = files.len();
        Some(Order {
            files,
            order: shuffled(n, shuffle),
            at: 0,
            looping: settings["looping"].as_bool().unwrap_or(true),
            shuffle,
        })
    }

    /// Follows new settings, keeping its place when the files are the same; whether the file
    /// playing changed.
    pub fn follow(&mut self, settings: &Value) -> bool {
        let Some(new) = Order::of(settings) else { return false };
        if new.files != self.files {
            *self = new;
            return true;
        }
        self.looping = new.looping;
        if self.shuffle != new.shuffle {
            let was = self.file().cloned();
            self.shuffle = new.shuffle;
            self.order = shuffled(self.files.len(), self.shuffle);
            self.at = 0;
            return self.file() != was.as_ref();
        }
        false
    }

    /// The file playing.
    pub fn file(&self) -> Option<&String> {
        self.order.get(self.at).and_then(|i| self.files.get(*i))
    }

    /// The file playing ended: on to the next (round again when looping); false at the end.
    pub fn ended(&mut self) -> bool {
        if self.at + 1 < self.order.len() {
            self.at += 1;
        } else if self.looping {
            self.at = 0;
            self.order = shuffled(self.files.len(), self.shuffle);
        } else {
            return false;
        }
        true
    }

    /// Moves `by` files (wrapping).
    pub fn skip(&mut self, by: i64) {
        let n = self.order.len().max(1) as i64;
        self.at = (((self.at as i64 + by) % n + n) % n) as usize;
    }

    /// (position from 1, count, the file playing).
    pub fn now(&self) -> Option<(usize, usize, String)> {
        Some((self.at + 1, self.order.len(), self.file()?.clone()))
    }
}

/// The play order of `n` files: in turn, or shuffled.
fn shuffled(n: usize, shuffle: bool) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    if shuffle {
        let mut seed =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64) | 1;
        for i in (1..n).rev() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            order.swap(i, (seed % (i as u64 + 1)) as usize);
        }
    }
    order
}
