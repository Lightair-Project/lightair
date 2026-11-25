//! Lightair's media player (macOS): video files decoded by the hardware decoder straight into
//! IOSurfaces, with no copy through memory, and their sound decoded as it comes due.
//!
//! AVFoundation reads the file (`AVAssetReader`): video as BGRA IOSurface-backed pixel buffers
//! (VideoToolbox decodes and converts on the GPU), sound as 32-bit float PCM at the engine's
//! rate. It plays as OBS's media source does, with the same settings (`local_file`, `looping`,
//! `restart_on_activate`, `clear_on_media_end`) and the media controls.
//!
//! Lightair's compositor draws the frame each player shows ([`picture`], which picks the frame
//! due on the player's clock), so pause, seek, looping and the end follow it. A track taking the
//! source's sound gets it straight into the mixer, each part pushed as it comes due on the media
//! clock; sound no track takes, the end and a restart go to the player's [`Elsewhere`] (the
//! Library's player, say).

use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString, c_void};
use std::ptr::NonNull;
use std::sync::{Arc, LazyLock, Weak};
use std::time::Duration;

use lightair_model::Id;
use objc2_core_foundation::CFRetained;
use objc2_core_video::CVPixelBuffer;

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{Encode, Encoding, RefEncode, msg_send};
use parking_lot::{Condvar, Mutex};



/// How far ahead of the clock sound no track takes is handed on (it's buffered by timestamp).
const AUDIO_AHEAD_NS: i64 = 150_000_000;
/// How far ahead of the clock sound is pushed into the mixer's feed: about what the feed keeps
/// buffered (1024 frames), so it's heard when it's due.
const FED_AHEAD_NS: i64 = 20_000_000;
/// Decoded frames waiting to be shown, at most (60 fps: about 70 ms).
const QUEUE: usize = 4;
/// Time to decode the first frames before the clock starts.
const PREROLL_NS: u64 = 60_000_000;
/// Frames a drawer may still be drawing from: kept (in use) for a few frames after replaced.
const HELD: usize = 3;
/// With Lightair's own compositor: more surfaces, for the frames its canvases may still be
/// drawing from (each holds one until the GPU has finished with it).
const COMPOSITOR_HELD: usize = 4;


/// A media state code, numbered as OBS's media states are (what the app and the remote API
/// read).
pub type MediaStateCode = u32;

/// The media states.
pub mod state {
    use super::MediaStateCode;
    pub const NONE: MediaStateCode = 0;
    pub const PLAYING: MediaStateCode = 1;
    pub const OPENING: MediaStateCode = 2;
    pub const BUFFERING: MediaStateCode = 3;
    pub const PAUSED: MediaStateCode = 4;
    pub const STOPPED: MediaStateCode = 5;
    pub const ENDED: MediaStateCode = 6;
    pub const ERROR: MediaStateCode = 7;
}

/// Where a player tells what happens besides the compositor and the mixer (the Library's player).
pub trait Elsewhere: Send + Sync {
    /// Sound no track of the mixer took: stereo, interleaved, at `rate`, shown at
    /// `timestamp_ns` (Lightair's clock), handed over ahead of time.
    fn audio(&self, interleaved: &[f32], rate: u32, timestamp_ns: u64);
    /// It played to the end.
    fn ended(&self);
    /// It started over (a restart, or play after the end).
    fn started(&self);
}

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
    fn CVPixelBufferLockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(buffer: *mut c_void) -> *mut c_void;
    fn CVPixelBufferGetDataSize(buffer: *mut c_void) -> usize;
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
    fn CFGetRetainCount(cf: *const c_void) -> isize;
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
/// converted into. In use while anything but the
/// reader's ring holds it (queued, or on screen and the frames just before it).
pub struct Slot {
    pub id: u64,
    pub buffer: *mut c_void,
    pub size: (u32, u32),
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
    pub fn surface(&self) -> *mut c_void {
        unsafe { CVPixelBufferGetIOSurface(self.buffer) }
    }

    /// A fully transparent slot (zeroed), never written again.
    fn blank(size: (u32, u32)) -> Option<Arc<Slot>> {
        let slot = Slot::new(size)?;
        unsafe {
            if CVPixelBufferLockBaseAddress(slot.buffer, 0) != 0 {
                return None;
            }
            let base = CVPixelBufferGetBaseAddress(slot.buffer);
            if !base.is_null() {
                std::ptr::write_bytes(base as *mut u8, 0, CVPixelBufferGetDataSize(slot.buffer));
            }
            CVPixelBufferUnlockBaseAddress(slot.buffer, 0);
        }
        Some(slot)
    }

    /// The pixel buffer, with a reference of its own (for the compositor).
    fn retained(&self) -> Option<CFRetained<CVPixelBuffer>> {
        // SAFETY: `buffer` is a live `CVPixelBufferRef`; `retain` takes a reference of its own.
        NonNull::new(self.buffer.cast::<CVPixelBuffer>()).map(|p| unsafe { CFRetained::retain(p) })
    }

    /// Whether anything outside the source holds its pixel buffer (the compositor drawing it).
    fn lent(&self) -> bool {
        unsafe { CFGetRetainCount(self.buffer) > 1 }
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
        // with the compositor, a slot it still draws from (holding its pixel buffer) isn't free
        let own = crate::show::own();
        let most = if own { SLOTS + COMPOSITOR_HELD } else { SLOTS };
        let slot = match self.slots.iter().find(|s| Arc::strong_count(s) == 1 && !(own && s.lent())) {
            Some(s) => s.clone(),
            None if self.slots.len() < most => {
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
pub enum Next {
    Show(Arc<Slot>),
    Clear,
}

/// What the source shows now, for Lightair's own compositor: set on Lightair's clock.
#[derive(Default)]
struct Composited {
    /// The frame shown (`None` before the first one, or once cleared).
    slot: Option<Arc<Slot>>,
    /// Shows nothing (cleared at the end or on a stop).
    cleared: bool,
    /// Counts the pictures shown: a different value is a new picture.
    serial: u64,
    /// The last frame's size, for the transparent picture shown once cleared.
    size: (u32, u32),
    /// The transparent picture of that size.
    blank: Option<Arc<Slot>>,
}

// ------------------------------------------------------------------ playback state

#[derive(Clone, PartialEq)]
pub struct Settings {
    pub path: String,
    pub looping: bool,
    pub restart_on_activate: bool,
    pub clear_on_end: bool,
}

struct Control {
    state: MediaStateCode,
    /// Bumped by every play, seek and stop: the reader starts over.
    run: u64,
    /// The reader should be reading (playing or paused).
    reading: bool,
    start_at: i64,
    /// The clock: the media time `base_media` was (or will be) at `base_os` (`crate::clock`).
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
            state::PLAYING => self.base_media + (now as i64 - self.base_os as i64),
            _ => self.base_media,
        }
    }
}

pub struct Shared {
    /// Where the end, a restart and sound no track takes are told.
    elsewhere: Option<Arc<dyn Elsewhere>>,
    rate: u32,
    control: Mutex<Control>,
    wake: Condvar,
    queue: Mutex<VecDeque<Frame>>,
    next: Mutex<Option<Next>>,
    /// What the compositor draws.
    composited: Mutex<Composited>,
    /// Off air, it waits to play (0 no, 1 from the start, 2 on from where it paused): a file
    /// that doesn't restart on air (`restart_on_activate` off) still costs nothing off air.
    on_air_play: std::sync::atomic::AtomicU8,
    /// Where the sound goes while a track of the mixer takes it.
    feed: Option<Arc<crate::audio_host::FeedSlot>>,
}

impl Shared {
    /// Shows nothing from the next frame (the end, or a stop, with `clear_on_media_end`).
    fn clear_picture(&self) {
        *self.next.lock() = Some(Next::Clear);
        if crate::show::own() {
            let mut c = self.composited.lock();
            c.slot = None;
            c.cleared = true;
            c.serial += 1;
        }
    }

    /// Plays from `at` (ns), keeping a pause.
    fn play_from(&self, at: i64, keep_pause: bool) {
        let mut c = self.control.lock();
        let paused = keep_pause && c.state == state::PAUSED;
        c.run += 1;
        c.reading = !c.settings.path.is_empty();
        c.start_at = at;
        c.base_media = at;
        c.base_os = crate::clock::now_ns() + PREROLL_NS;
        c.loop_offset = 0;
        c.state = match () {
            _ if !c.reading => state::NONE,
            _ if paused => state::PAUSED,
            _ => state::PLAYING,
        };
        self.queue.lock().clear();
        drop(c);
        self.wake.notify_all();
    }

    fn stop(&self) {
        let mut c = self.control.lock();
        c.run += 1;
        c.reading = false;
        c.state = state::STOPPED;
        let clear = c.settings.clear_on_end;
        drop(c);
        self.queue.lock().clear();
        if clear {
            self.clear_picture();
        }
        self.wake.notify_all();
    }

    fn pause(&self, pause: bool) {
        let mut c = self.control.lock();
        let now = crate::clock::now_ns();
        match c.state {
            state::PLAYING if pause => {
                c.base_media = c.clock(now);
                c.state = state::PAUSED;
            }
            state::PAUSED if !pause => {
                c.base_os = now;
                c.state = state::PLAYING;
            }
            state::STOPPED | state::ENDED | state::NONE if !pause => {
                drop(c);
                self.play_from(0, false);
                if let Some(e) = &self.elsewhere {
                    e.started();
                }
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
            state::NONE | state::STOPPED => return 0,
            state::ENDED => return c.duration,
            _ => {}
        }
        let mut t = c.clock(crate::clock::now_ns()) - c.loop_offset;
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
    /// Frames already pushed into the mixer's feed.
    sent: usize,
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
            at.zip(data.flatten()).map(|(at, data)| Sound { at: at + offset, data, sent: 0 })
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
                c.state = state::ERROR;
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
                c.state = state::ERROR;
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
                let now = crate::clock::now_ns();
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
            // a track of the mixer takes the sound: the part due by now, into its feed
            let fed = shared.feed.as_ref().is_some_and(|slot| slot.attached());
            if state == state::PLAYING
                && fed
                && let Some(s) = sound.as_mut()
                && let Some(slot) = shared.feed.as_ref()
            {
                let total = s.data.len() / 2;
                let ahead = (now + FED_AHEAD_NS - s.at).max(0) as i128;
                let due = ((ahead * shared.rate as i128 / 1_000_000_000) as usize).min(total);
                if due > s.sent && slot.push(&s.data[s.sent * 2..due * 2], 2, shared.rate) {
                    s.sent = due;
                    moved = true;
                }
                if s.sent >= total {
                    sound = None;
                }
            }
            if state == state::PLAYING
                && !fed
                && let Some(s) = sound.as_ref().filter(|s| s.at - now < AUDIO_AHEAD_NS)
            {
                // (what a feed let go of mid-way didn't take)
                let rest = &s.data[s.sent * 2..];
                let at = base_os as i64 + (s.at - base_media) + (s.sent as i64 * 1_000_000_000 / shared.rate.max(1) as i64);
                if at > 0
                    && !rest.is_empty()
                    && let Some(e) = &shared.elsewhere
                {
                    e.audio(rest, shared.rate, at as u64);
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
                while !c.quit && c.run == run && c.clock(crate::clock::now_ns()) < end {
                    shared.wake.wait_for(&mut c, Duration::from_millis(10));
                }
                if c.quit || c.run != run {
                    return;
                }
                c.state = state::ENDED;
                c.reading = false;
                let clear = c.settings.clear_on_end;
                drop(c);
                if clear {
                    shared.clear_picture();
                }
                if let Some(e) = &shared.elsewhere {
                    e.ended();
                }
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
    pub fn next_frame(&mut self) -> Option<(u32, lightair_render::texture::PixelBuffer)> {
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

// ------------------------------------------------------------------ what each source shows, for the compositor

/// The document's media sources this source type plays, by document id.
static SOURCES: LazyLock<Mutex<HashMap<Id, Weak<Shared>>>> = LazyLock::new(Default::default);


/// A frame a media source shows: a BGRA IOSurface-backed pixel buffer, opaque (so its colours
/// are premultiplied too), or fully transparent once the source has cleared.
pub struct Picture {
    pub buffer: CFRetained<CVPixelBuffer>,
    /// Counts the source's pictures: a different value is a new picture.
    pub serial: u64,
}

/// Whether the document media source `source` plays through this source type now (a file
/// AVFoundation plays), so its frames can be drawn from [`picture`]. Any thread.
pub fn plays(source: Id) -> bool {
    SOURCES.lock().get(&source).is_some_and(|s| s.strong_count() > 0)
}

/// The frame the document media source `source` shows now, picked on its clock (so pause, seek, looping, a new file and the end follow the source).
/// `None` before its first frame, and for a file with a rotation (not turned upright here).
/// The surface isn't written again while the buffer is held. Any thread.
pub fn picture(source: Id) -> Option<Picture> {
    let shared = SOURCES.lock().get(&source)?.upgrade()?;
    if shared.control.lock().rotation != 0 {
        return None;
    }
    tick(&shared);
    let mut c = shared.composited.lock();
    let slot = match (&c.slot, c.cleared) {
        (Some(slot), _) => slot.clone(),
        (None, true) if c.size.0 > 0 && c.size.1 > 0 => {
            if c.blank.as_ref().is_none_or(|b| b.size != c.size) {
                c.blank = Slot::blank(c.size);
            }
            c.blank.clone()?
        }
        _ => return None,
    };
    let serial = c.serial;
    drop(c);
    Some(Picture { buffer: slot.retained()?, serial })
}

// ------------------------------------------------------------------ the player

impl Settings {
    /// From a media source's settings as the document keeps them.
    pub fn from_json(s: &serde_json::Value) -> Settings {
        let flag = |k: &str, d: bool| s.get(k).and_then(serde_json::Value::as_bool).unwrap_or(d);
        Settings {
            path: s.get("local_file").and_then(serde_json::Value::as_str).unwrap_or_default().to_string(),
            looping: flag("looping", false),
            restart_on_activate: flag("restart_on_activate", true),
            clear_on_end: flag("clear_on_media_end", true),
        }
    }
}

/// A media file played for a source, on a reader thread of its own, from when it's made until
/// it's dropped.
pub struct MediaPlayer {
    shared: Arc<Shared>,
    source: Option<Id>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MediaPlayer {
    /// A player for document source `source` (`None`: a private player, the Library's), with
    /// `settings`, its sound at `rate`. `play_now`: it plays at once (on air, or private); else
    /// it waits to be shown ([`MediaPlayer::activate`]).
    pub fn new(
        source: Option<Id>,
        settings: Settings,
        rate: u32,
        play_now: bool,
        elsewhere: Option<Arc<dyn Elsewhere>>,
    ) -> MediaPlayer {
        let feed = source.filter(|_| crate::show::own() && crate::audio_host::mixer_on()).map(crate::audio_host::feed_slot);
        let restart = settings.restart_on_activate;
        let shared = Arc::new(Shared {
            elsewhere,
            rate,
            control: Mutex::new(Control {
                state: state::NONE,
                run: 0,
                reading: false,
                start_at: 0,
                base_os: 0,
                base_media: 0,
                loop_offset: 0,
                duration: 0,
                natural: (0, 0),
                rotation: 0,
                settings,
                quit: false,
            }),
            wake: Condvar::new(),
            queue: Mutex::new(VecDeque::new()),
            next: Mutex::new(None),
            composited: Mutex::new(Composited::default()),
            on_air_play: Default::default(),
            feed,
        });
        // a document's source is found by the compositor
        if crate::show::own()
            && let Some(id) = source
        {
            SOURCES.lock().insert(id, Arc::downgrade(&shared));
        }
        let thread = {
            let shared = shared.clone();
            std::thread::Builder::new().name("lightair-media".into()).spawn(move || reader_thread(shared)).ok()
        };
        if play_now {
            shared.play_from(0, false);
        } else if !restart {
            shared.on_air_play.store(1, std::sync::atomic::Ordering::Relaxed);
        }
        MediaPlayer { shared, source, thread }
    }

    /// New settings; a new file plays from the start when it's on air (`on_air`) or doesn't wait
    /// to be.
    pub fn update(&self, s: Settings, on_air: bool) {
        let mut c = self.shared.control.lock();
        let reopen = c.settings.path != s.path;
        c.settings = s.clone();
        drop(c);
        if reopen && (!s.restart_on_activate || on_air) {
            self.shared.play_from(0, false);
        }
    }

    /// On air: from the start (`restart_on_activate`; true, it started over), or on from where it
    /// was off air.
    pub fn activate(&self) -> bool {
        if self.shared.control.lock().settings.restart_on_activate {
            self.shared.play_from(0, false);
            return true;
        }
        match self.shared.on_air_play.swap(0, std::sync::atomic::Ordering::AcqRel) {
            1 => self.shared.play_from(0, false),
            2 => self.shared.pause(false),
            _ => {}
        }
        false
    }

    /// Off air: stopped (`restart_on_activate`), or paused (nothing decoded) until back on air.
    pub fn deactivate(&self) {
        if self.shared.control.lock().settings.restart_on_activate {
            self.shared.stop();
            return;
        }
        if self.shared.control.lock().state == state::PLAYING {
            self.shared.pause(true);
            self.shared.on_air_play.store(2, std::sync::atomic::Ordering::Release);
        }
    }

    /// Picks the frame due now: for the compositor's picture and a drawer's [`take_next`].
    pub fn tick(&self) {
        tick(&self.shared);
    }

    /// For a drawer other than the compositor: what to show next, once picked.
    pub fn take_next(&self) -> Option<Next> {
        self.shared.next.lock().take()
    }

    /// The picture's size as stored, and its rotation in degrees (clockwise).
    pub fn natural(&self) -> ((u32, u32), u32) {
        let c = self.shared.control.lock();
        (c.natural, c.rotation)
    }

    pub fn play_pause(&self, pause: bool) {
        self.shared.pause(pause);
    }

    /// From the start.
    pub fn restart(&self) {
        self.shared.play_from(0, false);
    }

    pub fn stop(&self) {
        self.shared.stop();
    }

    pub fn duration_ms(&self) -> i64 {
        self.shared.control.lock().duration / 1_000_000
    }

    pub fn time_ms(&self) -> i64 {
        self.shared.time() / 1_000_000
    }

    /// Goes to `ms` (while a file is open; a pause stays).
    pub fn set_time_ms(&self, ms: i64) {
        let (reading, duration) = {
            let c = self.shared.control.lock();
            (c.reading, c.duration)
        };
        if reading {
            let at = (ms.max(0) * 1_000_000).min(duration.max(0));
            self.shared.play_from(at, true);
        }
    }

    pub fn state(&self) -> MediaStateCode {
        self.shared.control.lock().state
    }
}

impl Drop for MediaPlayer {
    fn drop(&mut self) {
        if let Some(id) = self.source
            && self.shared.feed.is_some()
        {
            crate::audio_host::feeds_itself_no_more(id);
        }
        let me = Arc::downgrade(&self.shared);
        SOURCES.lock().retain(|_, s| !Weak::ptr_eq(s, &me));
        self.shared.control.lock().quit = true;
        self.shared.wake.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.shared.queue.lock().clear();
    }
}

/// Picks the frame due now on `shared`'s clock.
fn tick(shared: &Shared) {
    let (now, run) = {
        let c = shared.control.lock();
        (c.clock(crate::clock::now_ns()), c.run)
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
        {
            let mut c = shared.composited.lock();
            c.size = f.slot.size;
            c.slot = Some(f.slot.clone());
            c.cleared = false;
            c.serial += 1;
        }
        let mut next = shared.next.lock();
        if !matches!(*next, Some(Next::Clear)) {
            *next = Some(Next::Show(f.slot));
        }
        drop(next);
        shared.wake.notify_all();
    }
}
