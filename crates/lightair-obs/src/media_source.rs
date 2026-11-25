//! Lightair's own media source on macOS (`lightair_media`): video files decoded by the
//! hardware decoder straight into IOSurfaces that libobs draws as textures, with no copy through
//! memory. OBS's media source (`ffmpeg_source`) hands libobs its frames in memory, so even with
//! hardware decoding each frame comes back from the decoder and is uploaded again (measured in
//! docs/benchmarks/lightair-vs-obs.md).
//!
//! AVFoundation reads the file (`AVAssetReader`): video as BGRA IOSurface-backed pixel buffers
//! (VideoToolbox decodes and converts on the GPU), sound as 32-bit float PCM at libobs's rate,
//! handed to libobs with timestamps on the same clock the frames are shown by.
//!
//! It plays as OBS's media source does, with the same settings (`local_file`, `looping`,
//! `restart_on_activate`, `clear_on_media_end`) and the media controls; [`crate::kinds`]
//! routes the files AVFoundation plays (local MP4 and QuickTime files) here.

use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::Arc;
use std::time::Duration;

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{Encode, Encoding, RefEncode, msg_send};
use parking_lot::{Condvar, Mutex};

use crate::sys;

/// The libobs source type.
pub const ID: &CStr = c"lightair_media";

/// How far ahead of the clock sound is handed to libobs (it buffers it by timestamp).
const AUDIO_AHEAD_NS: i64 = 150_000_000;
/// Decoded frames waiting to be shown, at most (60 fps: about 70 ms).
const QUEUE: usize = 4;
/// Time to decode the first frames before the clock starts.
const PREROLL_NS: u64 = 60_000_000;
/// Frames libobs may still be drawing from: kept (in use) for a few frames after replaced.
const HELD: usize = 3;

// ------------------------------------------------------------------ Apple frameworks

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}
// SAFETY: CMTime's Objective-C encoding ({?=qiIq}).
unsafe impl Encode for CMTime {
    const ENCODING: Encoding = Encoding::Struct("?", &[i64::ENCODING, i32::ENCODING, u32::ENCODING, i64::ENCODING]);
}

impl CMTime {
    const VALID: u32 = 1;
    fn from_ns(ns: i64) -> Self {
        CMTime { value: ns, timescale: 1_000_000_000, flags: Self::VALID, epoch: 0 }
    }
    fn positive_infinity() -> Self {
        CMTime { value: 0, timescale: 0, flags: Self::VALID | 4, epoch: 0 }
    }
    fn ns(&self) -> Option<i64> {
        (self.flags & Self::VALID != 0 && self.flags & 0x1C == 0 && self.timescale > 0)
            .then(|| (self.value as i128 * 1_000_000_000 / self.timescale as i128) as i64)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTimeRange {
    start: CMTime,
    duration: CMTime,
}
// SAFETY: CMTimeRange's Objective-C encoding ({?={?=qiIq}{?=qiIq}}).
unsafe impl Encode for CMTimeRange {
    const ENCODING: Encoding = Encoding::Struct("?", &[CMTime::ENCODING, CMTime::ENCODING]);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}
// SAFETY: CGSize's Objective-C encoding.
unsafe impl Encode for CGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGAffineTransform {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    tx: f64,
    ty: f64,
}
// SAFETY: CGAffineTransform's Objective-C encoding.
unsafe impl Encode for CGAffineTransform {
    const ENCODING: Encoding = Encoding::Struct("CGAffineTransform", &[f64::ENCODING; 6]);
}

/// A `CMSampleBufferRef`'s target.
#[repr(C)]
struct OpaqueSampleBuffer {
    _p: [u8; 0],
}
// SAFETY: CMSampleBufferRef's Objective-C encoding (^{opaqueCMSampleBuffer=}).
unsafe impl RefEncode for OpaqueSampleBuffer {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("opaqueCMSampleBuffer", &[]));
}

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {
    static AVMediaTypeVideo: *const AnyObject;
    static AVMediaTypeAudio: *const AnyObject;
    static AVFormatIDKey: *const AnyObject;
    static AVSampleRateKey: *const AnyObject;
    static AVNumberOfChannelsKey: *const AnyObject;
    static AVLinearPCMBitDepthKey: *const AnyObject;
    static AVLinearPCMIsFloatKey: *const AnyObject;
    static AVLinearPCMIsNonInterleaved: *const AnyObject;
    static AVLinearPCMIsBigEndianKey: *const AnyObject;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: *const AnyObject;
    static kCVPixelBufferIOSurfacePropertiesKey: *const AnyObject;
    static kCVPixelBufferMetalCompatibilityKey: *const AnyObject;
    fn CVPixelBufferGetIOSurface(buffer: *mut c_void) -> *mut c_void;
    fn CVPixelBufferGetWidth(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetHeight(buffer: *mut c_void) -> usize;
    fn CVPixelBufferCreate(
        allocator: *const c_void,
        width: usize,
        height: usize,
        format: u32,
        attributes: *const c_void,
        out: *mut *mut c_void,
    ) -> i32;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    fn VTPixelTransferSessionCreate(allocator: *const c_void, out: *mut *mut c_void) -> i32;
    fn VTPixelTransferSessionTransferImage(session: *mut c_void, source: *mut c_void, destination: *mut c_void) -> i32;
    fn VTPixelTransferSessionInvalidate(session: *mut c_void);
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSampleBufferGetImageBuffer(buffer: *mut OpaqueSampleBuffer) -> *mut c_void;
    fn CMSampleBufferGetPresentationTimeStamp(buffer: *mut OpaqueSampleBuffer) -> CMTime;
    fn CMSampleBufferGetDataBuffer(buffer: *mut OpaqueSampleBuffer) -> *mut c_void;
    fn CMBlockBufferGetDataLength(buffer: *mut c_void) -> usize;
    fn CMBlockBufferCopyDataBytes(buffer: *mut c_void, offset: usize, length: usize, destination: *mut c_void) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

const BGRA: u32 = u32::from_be_bytes(*b"BGRA");
/// The decoder's own format (8-bit 4:2:0, video range): no conversion inside the reader.
const NV12_VIDEO: u32 = u32::from_be_bytes(*b"420v");
const LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
/// `AVAssetReaderStatus`.
const READER_READING: isize = 1;

fn class(name: &CStr) -> &'static AnyClass {
    AnyClass::get(name).unwrap_or_else(|| panic!("{name:?} (AVFoundation) is missing"))
}

/// An autoreleased object, kept.
unsafe fn keep(object: *mut AnyObject) -> Option<Retained<AnyObject>> {
    unsafe { Retained::retain(object) }
}

unsafe fn number_u32(v: u32) -> *mut AnyObject {
    unsafe { msg_send![class(c"NSNumber"), numberWithUnsignedInt: v] }
}

unsafe fn number_bool(v: bool) -> *mut AnyObject {
    unsafe { msg_send![class(c"NSNumber"), numberWithBool: v] }
}

unsafe fn dictionary(pairs: &[(*const AnyObject, *const AnyObject)]) -> *mut AnyObject {
    unsafe {
        let d: *mut AnyObject = msg_send![class(c"NSMutableDictionary"), dictionary];
        for &(k, v) in pairs {
            let _: () = msg_send![d, setObject: v, forKey: k];
        }
        d
    }
}

// ------------------------------------------------------------------ frames

/// One of a source's own BGRA IOSurfaces (a retained `CVPixelBuffer`) that decoded frames are
/// converted into, with a texture libobs makes over it once. In use while anything but the
/// reader's ring holds it (queued, or on screen and the frames just before it).
struct Slot {
    id: u64,
    buffer: *mut c_void,
    size: (u32, u32),
}

// SAFETY: pixel buffers and IOSurfaces are reference-counted, usable from any thread.
unsafe impl Send for Slot {}
unsafe impl Sync for Slot {}

impl Slot {
    fn new(size: (u32, u32)) -> Option<Arc<Slot>> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut buffer: *mut c_void = std::ptr::null_mut();
        let rc = autoreleasepool(|_| unsafe {
            let attrs = dictionary(&[
                (kCVPixelBufferIOSurfacePropertiesKey, dictionary(&[])),
                (kCVPixelBufferMetalCompatibilityKey, number_bool(true)),
            ]);
            CVPixelBufferCreate(std::ptr::null(), size.0 as usize, size.1 as usize, BGRA, attrs as *const c_void, &mut buffer)
        });
        if rc != 0 || buffer.is_null() {
            tracing::error!(rc, ?size, "media: no picture buffer");
            return None;
        }
        Some(Arc::new(Slot { id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed), buffer, size }))
    }
    fn surface(&self) -> *mut c_void {
        unsafe { CVPixelBufferGetIOSurface(self.buffer) }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        unsafe { CFRelease(self.buffer) };
    }
}

/// A decoder's frame (a retained `CVPixelBuffer` in its own format and pool), before it's
/// converted into a slot.
struct Decoded {
    buffer: *mut c_void,
    at: i64,
}

impl Drop for Decoded {
    fn drop(&mut self) {
        unsafe { CFRelease(self.buffer) };
    }
}

/// The reader's slots: queue + on screen + spare. A slot is free when only the ring holds it.
const SLOTS: usize = QUEUE + HELD + 1;

/// The reader thread's slots and its converter (VideoToolbox, on the GPU).
struct Ring {
    slots: Vec<Arc<Slot>>,
    transfer: *mut c_void,
}

impl Ring {
    fn new() -> Self {
        let mut transfer: *mut c_void = std::ptr::null_mut();
        let rc = unsafe { VTPixelTransferSessionCreate(std::ptr::null(), &mut transfer) };
        if rc != 0 {
            tracing::error!(rc, "media: VTPixelTransferSessionCreate failed");
        }
        Ring { slots: Vec::new(), transfer }
    }

    /// `frame` converted into a free slot; `None` while every slot is in use.
    fn convert(&mut self, frame: &Decoded) -> Option<Arc<Slot>> {
        let size = unsafe { (CVPixelBufferGetWidth(frame.buffer) as u32, CVPixelBufferGetHeight(frame.buffer) as u32) };
        self.slots.retain(|s| s.size == size);
        let slot = match self.slots.iter().find(|s| Arc::strong_count(s) == 1) {
            Some(s) => s.clone(),
            None if self.slots.len() < SLOTS => {
                let s = Slot::new(size)?;
                self.slots.push(s.clone());
                s
            }
            None => return None,
        };
        if self.transfer.is_null() {
            return None;
        }
        let rc = unsafe { VTPixelTransferSessionTransferImage(self.transfer, frame.buffer, slot.buffer) };
        (rc == 0).then_some(slot)
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        if !self.transfer.is_null() {
            unsafe {
                VTPixelTransferSessionInvalidate(self.transfer);
                CFRelease(self.transfer);
            }
        }
    }
}

struct Frame {
    slot: Arc<Slot>,
    /// When it shows, on the media clock (ns), loops included.
    at: i64,
    /// The play request it belongs to (frames of an earlier one are dropped).
    run: u64,
}

/// What `video_render` takes next.
enum Next {
    Show(Arc<Slot>),
    Clear,
}

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

// ------------------------------------------------------------------ playback state

#[derive(Clone, PartialEq)]
struct Settings {
    path: String,
    looping: bool,
    restart_on_activate: bool,
    clear_on_end: bool,
}

struct Control {
    state: sys::obs_media_state,
    /// Bumped by every play, seek and stop: the reader starts over.
    run: u64,
    /// The reader should be reading (playing or paused).
    reading: bool,
    start_at: i64,
    /// The clock: the media time `base_media` was (or will be) at `base_os` (os_gettime_ns).
    base_os: u64,
    base_media: i64,
    /// Media time added by the loops so far.
    loop_offset: i64,
    duration: i64,
    /// The picture's size as stored, and its rotation in degrees (clockwise).
    natural: (u32, u32),
    rotation: u32,
    settings: Settings,
    quit: bool,
}

impl Control {
    fn clock(&self, now: u64) -> i64 {
        match self.state {
            sys::OBS_MEDIA_STATE_PLAYING => self.base_media + (now as i64 - self.base_os as i64),
            _ => self.base_media,
        }
    }
}

struct Shared {
    source: usize,
    rate: u32,
    control: Mutex<Control>,
    wake: Condvar,
    queue: Mutex<VecDeque<Frame>>,
    next: Mutex<Option<Next>>,
    shown: Mutex<Shown>,
    /// Off air, it waits to play (0 no, 1 from the start, 2 on from where it paused): a file
    /// that doesn't restart on air (`restart_on_activate` off) still costs nothing off air.
    on_air_play: std::sync::atomic::AtomicU8,
}

impl Shared {
    fn source(&self) -> *mut sys::obs_source_t {
        self.source as *mut sys::obs_source_t
    }

    /// Plays from `at` (ns), keeping a pause.
    fn play_from(&self, at: i64, keep_pause: bool) {
        let mut c = self.control.lock();
        let paused = keep_pause && c.state == sys::OBS_MEDIA_STATE_PAUSED;
        c.run += 1;
        c.reading = !c.settings.path.is_empty();
        c.start_at = at;
        c.base_media = at;
        c.base_os = unsafe { sys::os_gettime_ns() } + PREROLL_NS;
        c.loop_offset = 0;
        c.state = match () {
            _ if !c.reading => sys::OBS_MEDIA_STATE_NONE,
            _ if paused => sys::OBS_MEDIA_STATE_PAUSED,
            _ => sys::OBS_MEDIA_STATE_PLAYING,
        };
        self.queue.lock().clear();
        drop(c);
        self.wake.notify_all();
    }

    fn stop(&self) {
        let mut c = self.control.lock();
        c.run += 1;
        c.reading = false;
        c.state = sys::OBS_MEDIA_STATE_STOPPED;
        let clear = c.settings.clear_on_end;
        drop(c);
        self.queue.lock().clear();
        if clear {
            *self.next.lock() = Some(Next::Clear);
        }
        self.wake.notify_all();
    }

    fn pause(&self, pause: bool) {
        let mut c = self.control.lock();
        let now = unsafe { sys::os_gettime_ns() };
        match c.state {
            sys::OBS_MEDIA_STATE_PLAYING if pause => {
                c.base_media = c.clock(now);
                c.state = sys::OBS_MEDIA_STATE_PAUSED;
            }
            sys::OBS_MEDIA_STATE_PAUSED if !pause => {
                c.base_os = now;
                c.state = sys::OBS_MEDIA_STATE_PLAYING;
            }
            sys::OBS_MEDIA_STATE_STOPPED | sys::OBS_MEDIA_STATE_ENDED | sys::OBS_MEDIA_STATE_NONE if !pause => {
                drop(c);
                self.play_from(0, false);
                unsafe { sys::obs_source_media_started(self.source()) };
                return;
            }
            _ => {}
        }
        drop(c);
        self.wake.notify_all();
    }

    /// The position shown now (ns), within the file.
    fn time(&self) -> i64 {
        let c = self.control.lock();
        match c.state {
            sys::OBS_MEDIA_STATE_NONE | sys::OBS_MEDIA_STATE_STOPPED => return 0,
            sys::OBS_MEDIA_STATE_ENDED => return c.duration,
            _ => {}
        }
        let mut t = c.clock(unsafe { sys::os_gettime_ns() }) - c.loop_offset;
        if t < 0 && c.duration > 0 && c.loop_offset > 0 {
            t += c.duration;
        }
        t.clamp(0, c.duration.max(0))
    }
}

// ------------------------------------------------------------------ the reader thread

struct Asset {
    path: String,
    asset: Retained<AnyObject>,
    video: Option<Retained<AnyObject>>,
    audio: Option<Retained<AnyObject>>,
}

struct Reader {
    reader: Retained<AnyObject>,
    video: Option<Retained<AnyObject>>,
    audio: Option<Retained<AnyObject>>,
}

impl Drop for Reader {
    fn drop(&mut self) {
        unsafe {
            let status: isize = msg_send![&*self.reader, status];
            if status == READER_READING {
                let _: () = msg_send![&*self.reader, cancelReading];
            }
        }
    }
}

fn first_track(asset: &AnyObject, kind: *const AnyObject) -> Option<Retained<AnyObject>> {
    unsafe {
        let tracks: *mut AnyObject = msg_send![asset, tracksWithMediaType: kind];
        if tracks.is_null() {
            return None;
        }
        let first: *mut AnyObject = msg_send![tracks, firstObject];
        keep(first)
    }
}

fn open_asset(path: &str) -> Option<Asset> {
    let c_path = CString::new(path).ok()?;
    unsafe {
        let string: *mut AnyObject = msg_send![class(c"NSString"), stringWithUTF8String: c_path.as_ptr()];
        let url: *mut AnyObject = msg_send![class(c"NSURL"), fileURLWithPath: string];
        let asset: *mut AnyObject = msg_send![class(c"AVURLAsset"), URLAssetWithURL: url, options: std::ptr::null_mut::<AnyObject>()];
        let asset = keep(asset)?;
        let video = first_track(&asset, AVMediaTypeVideo);
        let audio = first_track(&asset, AVMediaTypeAudio);
        (video.is_some() || audio.is_some()).then(|| Asset { path: path.to_owned(), asset, video, audio })
    }
}

/// A reader of `asset` from `start` (ns) on.
fn open_reader(asset: &Asset, start: i64, rate: u32) -> Option<Reader> {
    unsafe {
        let mut error: *mut AnyObject = std::ptr::null_mut();
        let reader: *mut AnyObject = msg_send![class(c"AVAssetReader"), assetReaderWithAsset: &*asset.asset, error: &mut error];
        let reader = keep(reader)?;
        let range = CMTimeRange { start: CMTime::from_ns(start.max(0)), duration: CMTime::positive_infinity() };
        let _: () = msg_send![&*reader, setTimeRange: range];
        let output = |track: &AnyObject, settings: *mut AnyObject| -> Option<Retained<AnyObject>> {
            let out: *mut AnyObject =
                msg_send![class(c"AVAssetReaderTrackOutput"), assetReaderTrackOutputWithTrack: track, outputSettings: settings];
            let out = keep(out)?;
            let _: () = msg_send![&*out, setAlwaysCopiesSampleData: false];
            let ok: bool = msg_send![&*reader, canAddOutput: &*out];
            if !ok {
                return None;
            }
            let _: () = msg_send![&*reader, addOutput: &*out];
            Some(out)
        };
        // the decoder's own format, in its own pool (converted into the source's surfaces)
        let video = asset.video.as_deref().and_then(|t| {
            let settings = dictionary(&[
                (kCVPixelBufferPixelFormatTypeKey, number_u32(NV12_VIDEO)),
                (kCVPixelBufferIOSurfacePropertiesKey, dictionary(&[])),
                (kCVPixelBufferMetalCompatibilityKey, number_bool(true)),
            ]);
            output(t, settings)
        });
        let audio = asset.audio.as_deref().and_then(|t| {
            let settings = dictionary(&[
                (AVFormatIDKey, number_u32(LINEAR_PCM)),
                (AVSampleRateKey, number_u32(rate)),
                (AVNumberOfChannelsKey, number_u32(2)),
                (AVLinearPCMBitDepthKey, number_u32(32)),
                (AVLinearPCMIsFloatKey, number_bool(true)),
                (AVLinearPCMIsNonInterleaved, number_bool(false)),
                (AVLinearPCMIsBigEndianKey, number_bool(false)),
            ]);
            output(t, settings)
        });
        if video.is_none() && audio.is_none() {
            return None;
        }
        let started: bool = msg_send![&*reader, startReading];
        started.then_some(Reader { reader, video, audio })
    }
}

/// The next sample of `output` (+1, released by the caller), or `None` at the end.
fn next_sample(output: &AnyObject) -> Option<*mut OpaqueSampleBuffer> {
    let sample: *mut OpaqueSampleBuffer = unsafe { msg_send![output, copyNextSampleBuffer] };
    (!sample.is_null()).then_some(sample)
}

/// A decoded sound sample: when (ns, media clock) and stereo float frames.
struct Sound {
    at: i64,
    data: Vec<f32>,
}

fn read_video(output: &AnyObject, offset: i64) -> Option<Option<Decoded>> {
    autoreleasepool(|_| {
        let sample = next_sample(output)?;
        let frame = unsafe {
            let at = CMSampleBufferGetPresentationTimeStamp(sample).ns();
            let image = CMSampleBufferGetImageBuffer(sample);
            if !image.is_null() {
                CFRetain(image);
            }
            CFRelease(sample as *const c_void);
            (!image.is_null()).then_some(Decoded { buffer: image, at: at.unwrap_or(0) + offset }).filter(|_| at.is_some())
        };
        Some(frame)
    })
}

fn read_audio(output: &AnyObject, offset: i64) -> Option<Option<Sound>> {
    autoreleasepool(|_| {
        let sample = next_sample(output)?;
        let sound = unsafe {
            let at = CMSampleBufferGetPresentationTimeStamp(sample).ns();
            let block = CMSampleBufferGetDataBuffer(sample);
            let data = (!block.is_null()).then(|| {
                let bytes = CMBlockBufferGetDataLength(block);
                let mut data = vec![0f32; bytes / 4];
                let rc = CMBlockBufferCopyDataBytes(block, 0, data.len() * 4, data.as_mut_ptr() as *mut c_void);
                (rc == 0).then_some(data)
            });
            CFRelease(sample as *const c_void);
            at.zip(data.flatten()).map(|(at, data)| Sound { at: at + offset, data })
        };
        Some(sound)
    })
}

/// Opens the file (once per path), learning its size, rotation and length.
fn ensure_asset(shared: &Shared, asset: &mut Option<Asset>, path: &str) -> bool {
    if asset.as_ref().is_some_and(|a| a.path == path) {
        return true;
    }
    *asset = autoreleasepool(|_| open_asset(path));
    let Some(a) = asset else {
        tracing::warn!(path, "media: AVFoundation can't open the file");
        return false;
    };
    let (duration, natural, rotation) = unsafe {
        let duration: CMTime = msg_send![&*a.asset, duration];
        let (natural, rotation) = a.video.as_deref().map_or(((0, 0), 0), |t| {
            let size: CGSize = msg_send![t, naturalSize];
            let m: CGAffineTransform = msg_send![t, preferredTransform];
            let degrees = (m.b.atan2(m.a).to_degrees().round() as i32).rem_euclid(360) as u32;
            ((size.width.round() as u32, size.height.round() as u32), degrees / 90 * 90)
        });
        (duration.ns().unwrap_or(0), natural, rotation)
    };
    let mut c = shared.control.lock();
    c.duration = duration;
    c.natural = natural;
    c.rotation = rotation;
    true
}

fn reader_thread(shared: Arc<Shared>) {
    let mut asset: Option<Asset> = None;
    let mut ring = Ring::new();
    let mut done = 0;
    loop {
        let (run, start, settings) = {
            let mut c = shared.control.lock();
            while !c.quit && !(c.reading && c.run != done) {
                shared.wake.wait(&mut c);
            }
            if c.quit {
                return;
            }
            done = c.run;
            (c.run, c.start_at, c.settings.clone())
        };
        if !ensure_asset(&shared, &mut asset, &settings.path) {
            let mut c = shared.control.lock();
            if c.run == run {
                c.state = sys::OBS_MEDIA_STATE_ERROR;
                c.reading = false;
            }
            continue;
        }
        play(&shared, asset.as_ref().unwrap(), &mut ring, run, start);
    }
}

/// Reads from `start` until the play request `run` is replaced, or the end (looping on).
fn play(shared: &Shared, asset: &Asset, ring: &mut Ring, run: u64, start: i64) {
    let mut offset = 0;
    let mut reader = open_reader(asset, start, shared.rate);
    let (mut video, mut sound) = (None::<Decoded>, None::<Sound>);
    loop {
        let Some(r) = &reader else {
            let mut c = shared.control.lock();
            if c.run == run {
                c.state = sys::OBS_MEDIA_STATE_ERROR;
                c.reading = false;
            }
            return;
        };
        let (mut video_done, mut sound_done) = (r.video.is_none(), r.audio.is_none());
        loop {
            let (state, now, base_os, base_media, looping) = {
                let c = shared.control.lock();
                if c.quit || c.run != run {
                    return;
                }
                let now = unsafe { sys::os_gettime_ns() };
                (c.state, c.clock(now), c.base_os, c.base_media, c.settings.looping)
            };
            if video.is_none() && !video_done {
                match read_video(r.video.as_deref().unwrap(), offset) {
                    Some(f) => video = f,
                    None => video_done = true,
                }
            }
            if sound.is_none() && !sound_done {
                match read_audio(r.audio.as_deref().unwrap(), offset) {
                    Some(s) => sound = s,
                    None => sound_done = true,
                }
            }
            let mut moved = false;
            // into a free surface of the source's own once there's room in the queue (the decoder's
            // buffer goes straight back to it)
            if let Some(v) = &video
                && shared.queue.lock().len() < QUEUE
                && let Some(slot) = ring.convert(v)
            {
                shared.queue.lock().push_back(Frame { slot, at: v.at, run });
                video = None;
                moved = true;
            }
            if state == sys::OBS_MEDIA_STATE_PLAYING
                && let Some(s) = sound.as_ref().filter(|s| s.at - now < AUDIO_AHEAD_NS)
            {
                let at = base_os as i64 + (s.at - base_media);
                if at > 0 {
                    let audio = sys::obs_source_audio {
                        data: [s.data.as_ptr() as *const u8, ptr_null(), ptr_null(), ptr_null(), ptr_null(), ptr_null(), ptr_null(), ptr_null()],
                        frames: (s.data.len() / 2) as u32,
                        speakers: sys::SPEAKERS_STEREO,
                        format: sys::AUDIO_FORMAT_FLOAT,
                        samples_per_sec: shared.rate,
                        timestamp: at as u64,
                    };
                    unsafe { sys::obs_source_output_audio(shared.source(), &audio) };
                }
                sound = None;
                moved = true;
            }
            if video_done && sound_done && video.is_none() && sound.is_none() {
                if looping {
                    // on, seamlessly: the next pass's pictures and sound follow on the clock
                    let duration = shared.control.lock().duration;
                    offset += duration.max(1);
                    shared.control.lock().loop_offset = offset;
                    reader = open_reader(asset, 0, shared.rate);
                    break;
                }
                // the end: once the clock gets there
                let end = offset + shared.control.lock().duration;
                let mut c = shared.control.lock();
                while !c.quit && c.run == run && c.clock(unsafe { sys::os_gettime_ns() }) < end {
                    shared.wake.wait_for(&mut c, Duration::from_millis(10));
                }
                if c.quit || c.run != run {
                    return;
                }
                c.state = sys::OBS_MEDIA_STATE_ENDED;
                c.reading = false;
                let clear = c.settings.clear_on_end;
                drop(c);
                if clear {
                    *shared.next.lock() = Some(Next::Clear);
                }
                unsafe { sys::obs_source_media_ended(shared.source()) };
                return;
            }
            if !moved {
                let mut c = shared.control.lock();
                if c.run == run && !c.quit {
                    shared.wake.wait_for(&mut c, Duration::from_millis(4));
                }
            }
        }
    }
}

fn ptr_null() -> *const u8 {
    std::ptr::null()
}

// ------------------------------------------------------------------ a file's frames, for the compositor

/// A video file's frames in time order, as the decoder writes them: BGRA IOSurface-backed pixel
/// buffers that keep the file's alpha (ProRes 4444, HEVC with alpha), with no conversion. Lightair's
/// own compositor plays stinger videos with it. Made and read on one thread.
pub struct Frames {
    _asset: Retained<AnyObject>,
    reader: Retained<AnyObject>,
    output: Retained<AnyObject>,
    /// The file's length.
    pub duration_ms: u32,
}

impl Frames {
    /// The video of the file at `path`, from its start; `None` when AVFoundation can't read it
    /// (a container or codec it doesn't know, or no video).
    pub fn open(path: &str) -> Option<Frames> {
        autoreleasepool(|_| unsafe {
            let asset = open_asset(path)?;
            let track = asset.video.clone()?;
            let duration: CMTime = msg_send![&*asset.asset, duration];
            let mut error: *mut AnyObject = std::ptr::null_mut();
            let reader: *mut AnyObject =
                msg_send![class(c"AVAssetReader"), assetReaderWithAsset: &*asset.asset, error: &mut error];
            let reader = keep(reader)?;
            let settings = dictionary(&[
                (kCVPixelBufferPixelFormatTypeKey, number_u32(BGRA)),
                (kCVPixelBufferIOSurfacePropertiesKey, dictionary(&[])),
                (kCVPixelBufferMetalCompatibilityKey, number_bool(true)),
            ]);
            let output: *mut AnyObject = msg_send![
                class(c"AVAssetReaderTrackOutput"),
                assetReaderTrackOutputWithTrack: &*track,
                outputSettings: settings
            ];
            let output = keep(output)?;
            let _: () = msg_send![&*output, setAlwaysCopiesSampleData: false];
            let ok: bool = msg_send![&*reader, canAddOutput: &*output];
            if !ok {
                return None;
            }
            let _: () = msg_send![&*reader, addOutput: &*output];
            let started: bool = msg_send![&*reader, startReading];
            if !started {
                return None;
            }
            let duration_ms = (duration.ns().unwrap_or(0).max(0) / 1_000_000).min(u32::MAX as i64) as u32;
            Some(Frames { _asset: asset.asset, reader, output, duration_ms })
        })
    }

    /// The next frame and its time (ms from the file's start); `None` at the end.
    pub fn next(&mut self) -> Option<(u32, lightair_render::texture::PixelBuffer)> {
        loop {
            let decoded = read_video(&self.output, 0)?;
            let Some(decoded) = decoded else { continue };
            let at = (decoded.at.max(0) / 1_000_000).min(u32::MAX as i64) as u32;
            // the decoder's reference is handed to the pixel buffer, which releases it
            let decoded = std::mem::ManuallyDrop::new(decoded);
            // SAFETY: `buffer` is a retained `CVPixelBufferRef` whose reference is given up here.
            if let Some(buffer) = unsafe { lightair_render::texture::PixelBuffer::from_raw(decoded.buffer) } {
                return Some((at, buffer));
            }
        }
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        unsafe {
            let status: isize = msg_send![&*self.reader, status];
            if status == READER_READING {
                let _: () = msg_send![&*self.reader, cancelReading];
            }
        }
    }
}

// ------------------------------------------------------------------ the libobs source

struct Media {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
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
    let rate = if unsafe { sys::obs_get_audio_info(&mut info) } && info.samples_per_sec > 0 { info.samples_per_sec } else { 48000 };
    let s = settings_of(settings);
    let shared = Arc::new(Shared {
        source: source as usize,
        rate,
        control: Mutex::new(Control {
            state: sys::OBS_MEDIA_STATE_NONE,
            run: 0,
            reading: false,
            start_at: 0,
            base_os: 0,
            base_media: 0,
            loop_offset: 0,
            duration: 0,
            natural: (0, 0),
            rotation: 0,
            settings: s.clone(),
            quit: false,
        }),
        wake: Condvar::new(),
        queue: Mutex::new(VecDeque::new()),
        next: Mutex::new(None),
        shown: Mutex::new(Shown::default()),
        on_air_play: Default::default(),
    });
    let thread = {
        let shared = shared.clone();
        std::thread::Builder::new().name("lightair-media".into()).spawn(move || reader_thread(shared)).ok()
    };
    // plays once on air (from the start); a private one (the Library's player) at once
    if unsafe { sys::obs_source_active(source) || sys::obs_obj_is_private(source as *mut c_void) } {
        shared.play_from(0, false);
    } else if !s.restart_on_activate {
        shared.on_air_play.store(1, std::sync::atomic::Ordering::Relaxed);
    }
    Box::into_raw(Box::new(Media { shared, thread })) as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let mut m = unsafe { Box::from_raw(data as *mut Media) };
    m.shared.control.lock().quit = true;
    m.shared.wake.notify_all();
    if let Some(t) = m.thread.take() {
        let _ = t.join();
    }
    m.shared.queue.lock().clear();
    let mut shown = m.shared.shown.lock();
    unsafe {
        sys::obs_enter_graphics();
        shown.destroy();
        sys::obs_leave_graphics();
    }
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let m = media(data);
    let s = settings_of(settings);
    let mut c = m.shared.control.lock();
    let reopen = c.settings.path != s.path;
    c.settings = s.clone();
    drop(c);
    if reopen && (!s.restart_on_activate || unsafe { sys::obs_source_active(m.shared.source()) }) {
        m.shared.play_from(0, false);
    }
}

unsafe extern "C" fn activate(data: *mut c_void) {
    let m = media(data);
    if m.shared.control.lock().settings.restart_on_activate {
        m.shared.play_from(0, false);
        unsafe { sys::obs_source_media_started(m.shared.source()) };
        return;
    }
    // on from where it was off air (or from the start, the first time)
    match m.shared.on_air_play.swap(0, std::sync::atomic::Ordering::AcqRel) {
        1 => m.shared.play_from(0, false),
        2 => m.shared.pause(false),
        _ => {}
    }
}

unsafe extern "C" fn deactivate(data: *mut c_void) {
    let m = media(data);
    if m.shared.control.lock().settings.restart_on_activate {
        m.shared.stop();
        return;
    }
    // off air: paused (nothing decoded), on again when back on air
    if m.shared.control.lock().state == sys::OBS_MEDIA_STATE_PLAYING {
        m.shared.pause(true);
        m.shared.on_air_play.store(2, std::sync::atomic::Ordering::Release);
    }
}

/// Picks the frame due now (libobs's graphics thread, before rendering).
unsafe extern "C" fn video_tick(data: *mut c_void, _seconds: f32) {
    let shared = &media(data).shared;
    let (now, run) = {
        let c = shared.control.lock();
        (c.clock(unsafe { sys::os_gettime_ns() }), c.run)
    };
    let mut due = None;
    {
        let mut q = shared.queue.lock();
        while let Some(f) = q.front() {
            if f.run != run {
                q.pop_front();
            } else if f.at <= now {
                due = q.pop_front();
            } else {
                break;
            }
        }
    }
    if let Some(f) = due {
        let mut next = shared.next.lock();
        if !matches!(*next, Some(Next::Clear)) {
            *next = Some(Next::Show(f.slot));
        }
        drop(next);
        shared.wake.notify_all();
    }
}

unsafe extern "C" fn video_render(data: *mut c_void, _effect: *mut sys::gs_effect_t) {
    let shared = &media(data).shared;
    let mut shown = shared.shown.lock();
    match shared.next.lock().take() {
        Some(Next::Show(p)) => unsafe { shown.show(p) },
        Some(Next::Clear) => shown.clear(),
        None => {}
    }
    let Some(tex) = shown.texture() else { return };
    let (w, h) = shown.size;
    let rotation = shared.control.lock().rotation;
    unsafe {
        // an IOSurface is a rectangle texture on OpenGL (Metal has none)
        let rect = sys::gs_get_device_type() as u32 != sys::GS_DEVICE_METAL;
        let effect = sys::obs_get_base_effect(if rect { sys::OBS_EFFECT_DEFAULT_RECT } else { sys::OBS_EFFECT_DEFAULT });
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
    let shared = &media(data).shared;
    let shown = shared.shown.lock().size;
    let c = shared.control.lock();
    let (w, h) = if shown.0 > 0 { shown } else { c.natural };
    if c.rotation % 180 == 90 { (h, w) } else { (w, h) }
}

unsafe extern "C" fn get_width(data: *mut c_void) -> u32 {
    size(data).0
}

unsafe extern "C" fn get_height(data: *mut c_void) -> u32 {
    size(data).1
}

unsafe extern "C" fn media_play_pause(data: *mut c_void, pause: bool) {
    media(data).shared.pause(pause);
}

unsafe extern "C" fn media_restart(data: *mut c_void) {
    let m = media(data);
    m.shared.play_from(0, false);
    unsafe { sys::obs_source_media_started(m.shared.source()) };
}

unsafe extern "C" fn media_stop(data: *mut c_void) {
    media(data).shared.stop();
}

unsafe extern "C" fn media_get_duration(data: *mut c_void) -> i64 {
    media(data).shared.control.lock().duration / 1_000_000
}

unsafe extern "C" fn media_get_time(data: *mut c_void) -> i64 {
    media(data).shared.time() / 1_000_000
}

unsafe extern "C" fn media_set_time(data: *mut c_void, ms: i64) {
    let shared = &media(data).shared;
    let (reading, duration) = {
        let c = shared.control.lock();
        (c.reading, c.duration)
    };
    if reading {
        let at = (ms.max(0) * 1_000_000).min(duration.max(0));
        shared.play_from(at, true);
    }
}

unsafe extern "C" fn media_get_state(data: *mut c_void) -> sys::obs_media_state {
    media(data).shared.control.lock().state
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
