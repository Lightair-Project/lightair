//! macOS, with `LIGHTAIR_ENGINE=own`: a stinger transition's video for the compositor.
//!
//! A [`Player`] decodes its file on a thread of its own with the desktop's hardware path
//! (AVFoundation and VideoToolbox, `lightair_obs::media_source::Frames`): BGRA IOSurface-backed
//! frames with the file's alpha, drawn by the renderer with no copy. It starts reading when it's
//! made, as libobs preloads a stinger, so the first frames are ready when a switch starts; it
//! keeps a few frames ahead and hands the one for a time. After a switch it's rewound for the
//! next. A file AVFoundation can't read is `failed`: libobs draws that switch (its stinger reads
//! more formats), for the video's length when FFmpeg can tell it.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

use lightair_host::media_player::Frames;

/// A stinger's frames: AVFoundation's, else (WebM with alpha, and what else it can't read)
/// the `ffmpeg` tool's.
enum Reader {
    Av(Frames),
    Ff(lightair_host::ffmpeg_media::Frames),
}

impl Reader {
    fn open(path: &str) -> Option<Reader> {
        Frames::open(path).map(Reader::Av).or_else(|| lightair_host::ffmpeg_media::Frames::open(path).map(Reader::Ff))
    }

    fn duration_ms(&self) -> u32 {
        match self {
            Reader::Av(f) => f.duration_ms,
            Reader::Ff(f) => f.duration_ms,
        }
    }

    fn next_frame(&mut self) -> Option<(u32, lightair_render::texture::PixelBuffer)> {
        match self {
            Reader::Av(f) => f.next_frame(),
            Reader::Ff(f) => f.next_frame(),
        }
    }
}
use lightair_render::texture::PixelBuffer;
use parking_lot::{Condvar, Mutex};

/// Frames decoded ahead of the one shown (the decoder's own buffers: few, so it isn't held up).
const AHEAD: usize = 3;

pub(crate) struct Player {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Default)]
struct State {
    quit: bool,
    /// Bumped to read from the start again.
    run: u64,
    /// The frames read for `run`, oldest first: (time in ms, frame). The first is the one shown.
    frames: VecDeque<(u32, PixelBuffer)>,
    /// The file's end was reached in `run`.
    ended: bool,
    /// AVFoundation can't read the file.
    failed: bool,
    /// The video's length, once known.
    duration_ms: Option<u32>,
}

impl Player {
    /// Starts reading the stinger at `path`.
    pub(crate) fn new(path: &str) -> Player {
        let shared = Arc::new(Shared { state: Mutex::new(State::default()), wake: Condvar::new() });
        let thread = std::thread::Builder::new()
            .name("lightair-compose-stinger".into())
            .spawn({
                let shared = shared.clone();
                let path = path.to_string();
                move || read(&path, &shared)
            })
            .map_err(|e| tracing::error!(%e, "compositor: no thread for a stinger"))
            .ok();
        if thread.is_none() {
            shared.state.lock().failed = true;
        }
        Player { shared, thread }
    }

    /// AVFoundation can't read the file.
    pub(crate) fn failed(&self) -> bool {
        self.shared.state.lock().failed
    }

    /// The video's length, once the file is open (or, for one AVFoundation can't read, what
    /// FFmpeg says it is).
    pub(crate) fn duration_ms(&self) -> Option<u32> {
        self.shared.state.lock().duration_ms
    }

    /// The frame for `at_ms` into the video: the newest decoded at or before it (the first until
    /// then); `None` before the first is decoded.
    pub(crate) fn frame(&self, at_ms: u32) -> Option<PixelBuffer> {
        let mut s = self.shared.state.lock();
        while s.frames.len() > 1 && s.frames[1].0 <= at_ms {
            s.frames.pop_front();
        }
        let frame = s.frames.front().map(|(_, f)| f.clone());
        // room for the next ones
        self.shared.wake.notify_all();
        frame
    }

    /// Reads from the start again (the switch it played is over).
    pub(crate) fn rewind(&self) {
        let mut s = self.shared.state.lock();
        s.run += 1;
        s.frames.clear();
        s.ended = false;
        self.shared.wake.notify_all();
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.state.lock().quit = true;
        self.shared.wake.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The reading thread: opens the file for each run and keeps [`AHEAD`] frames past the shown
/// one, until the end.
fn read(path: &str, shared: &Shared) {
    // the run the open reader is for (none yet)
    let mut reading: Option<u64> = None;
    let mut frames: Option<Reader> = None;
    loop {
        let run = {
            let mut s = shared.state.lock();
            loop {
                if s.quit {
                    return;
                }
                if reading != Some(s.run) {
                    break;
                }
                if frames.is_some() && !s.ended && s.frames.len() <= AHEAD {
                    break;
                }
                shared.wake.wait(&mut s);
            }
            s.run
        };
        if reading != Some(run) {
            // from the start, for this run
            reading = Some(run);
            drop(frames.take());
            frames = Reader::open(path);
            let mut s = shared.state.lock();
            match &frames {
                Some(f) => {
                    s.failed = false;
                    s.duration_ms = Some(f.duration_ms());
                }
                None if !s.failed => {
                    tracing::warn!(
                        path,
                        "compositor: neither AVFoundation nor ffmpeg reads the stinger; libobs plays it"
                    );
                    s.failed = true;
                    s.ended = true;
                    drop(s);
                    // its length, for how long libobs draws the switch
                    let duration = lightair_render::media::probe(Path::new(path))
                        .ok()
                        .map(|p| p.duration_ms.min(u32::MAX as u64) as u32);
                    shared.state.lock().duration_ms = duration;
                }
                None => s.ended = true,
            }
            continue;
        }
        let next = frames.as_mut().and_then(Reader::next_frame);
        let mut s = shared.state.lock();
        if s.run != run {
            continue;
        }
        match next {
            Some(frame) => s.frames.push_back(frame),
            None => {
                s.ended = true;
                frames = None;
            }
        }
    }
}
