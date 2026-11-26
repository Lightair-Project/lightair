//! A local media file played by Media Foundation (Windows): its source reader decodes the video
//! on the GPU (the decoder draws into Direct3D textures on the adapter Lightair's buffers are on)
//! and the sound to float PCM at the mixer's rate. Each picture goes through the same conversion
//! the phone camera's decoder uses (NV12 to BGRA on the GPU's video processor, turned upright by
//! a rotation stored in the file) into a Lightair buffer, published by the source's id.
//!
//! One thread per run, reading the file's samples in order and handing each over when it's due
//! by the wall clock from where the run started. A file Media Foundation can't open, or one with
//! a stream it can't decode (VP9 or AV1 without Windows' extensions, say), is left to the
//! `ffmpeg` tool ([`super::FfmpegMedia`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lightair_model::Id;
use parking_lot::Mutex;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Variant::{VT_I8, VT_UI8};
use windows::core::{GUID, HSTRING};

use super::{Control, SoundTo, ended};
use crate::buffers::Copier;
use crate::phone_decode::{Gpu, Mf, aperture, in_memory, on_gpu};

/// How long opening a file may take before the `ffmpeg` tool is tried instead.
const OPENING: Duration = Duration::from_secs(3);

/// A late picture further behind than this is skipped (its sound plays on).
const LATE: Duration = Duration::from_millis(60);

/// What a run plays.
pub(super) struct Job {
    pub id: Id,
    pub path: String,
    pub from_ms: i64,
    pub looping: bool,
    pub rate: u32,
    pub stop: Arc<AtomicBool>,
    pub control: Arc<Mutex<Control>>,
    pub sound_to: Option<SoundTo>,
}

/// What an opened file is: its picture's size (turned upright; 0×0 for sound only) and length.
pub(super) type Opened = ((u32, u32), i64);

/// Starts playing `job` on a thread of its own; `None` when Media Foundation can't play the file
/// (nothing is left running).
pub(super) fn play(job: Job) -> Option<Opened> {
    let (tx, rx) = mpsc::channel();
    let stop = job.stop.clone();
    std::thread::Builder::new().name("lightair-mf-media".into()).spawn(move || run(job, tx)).ok()?;
    match rx.recv_timeout(OPENING) {
        Ok(opened) => opened,
        Err(_) => {
            // too slow (or gone): left to the tool, and this one stops once it opens
            stop.store(true, Ordering::Relaxed);
            None
        }
    }
}

/// A source reader on the file, its streams set to what Lightair takes.
struct Reader {
    reader: IMFSourceReader,
    video: bool,
    audio: bool,
    /// The decoded pictures' coded size and the part shown; the rotation stored in the file.
    coded: (u32, u32),
    shown: (u32, u32),
    rotation: u32,
    /// The sound as it comes out.
    channels: u32,
    rate: u32,
    duration_ms: i64,
    /// Which of the reader's streams is the video (by index; filled as samples come).
    kinds: Vec<Option<bool>>,
}

impl Reader {
    fn open(path: &str, gpu: Option<&Gpu>, rate: u32) -> Option<Reader> {
        let attributes = unsafe {
            let mut a = None;
            MFCreateAttributes(&mut a, 3).ok()?;
            a?
        };
        if let Some(g) = gpu {
            unsafe {
                attributes.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &g.manager).ok()?;
                let _ = attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1);
            }
        }
        let reader = unsafe { MFCreateSourceReaderFromURL(&HSTRING::from(path), &attributes) }.ok()?;
        let first_video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let first_audio = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        unsafe {
            let _ = reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
        }
        let mut r = Reader {
            reader,
            video: false,
            audio: false,
            coded: (0, 0),
            shown: (0, 0),
            rotation: 0,
            channels: 2,
            rate,
            duration_ms: 0,
            kinds: Vec::new(),
        };
        // the video, as NV12 (a stream it has but can't decode: the tool's)
        if let Ok(native) = unsafe { r.reader.GetNativeMediaType(first_video, 0) } {
            r.rotation = match unsafe { native.GetUINT32(&MF_MT_VIDEO_ROTATION) }.unwrap_or(0) {
                d @ (90 | 180 | 270) => d,
                _ => 0,
            };
            unsafe {
                let t = MFCreateMediaType().ok()?;
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok()?;
                t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).ok()?;
                if let Err(e) = r.reader.SetCurrentMediaType(first_video, None, &t) {
                    tracing::info!(%e, path, "media: Media Foundation can't decode its video");
                    return None;
                }
                r.reader.SetStreamSelection(first_video, true).ok()?;
            }
            r.video = true;
            r.video_type()?;
        }
        // the sound, as float stereo at the mixer's rate (or float as it comes)
        if unsafe { r.reader.GetNativeMediaType(first_audio, 0) }.is_ok() {
            let set = |full: bool| unsafe {
                let t = MFCreateMediaType().ok()?;
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).ok()?;
                t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float).ok()?;
                if full {
                    t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2).ok()?;
                    t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate).ok()?;
                    t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 32).ok()?;
                    t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 8).ok()?;
                    t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * 8).ok()?;
                    t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1).ok()?;
                }
                r.reader.SetCurrentMediaType(first_audio, None, &t).ok()
            };
            if set(true).is_none() && set(false).is_none() {
                tracing::info!(path, "media: Media Foundation can't decode its sound");
                return None;
            }
            unsafe { r.reader.SetStreamSelection(first_audio, true) }.ok()?;
            let t = unsafe { r.reader.GetCurrentMediaType(first_audio) }.ok()?;
            r.channels = unsafe { t.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS) }.unwrap_or(2).max(1);
            r.rate = unsafe { t.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND) }.unwrap_or(rate);
            r.audio = true;
        }
        if !r.video && !r.audio {
            return None;
        }
        if let Ok(p) = unsafe {
            r.reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
        } {
            // SAFETY: the union is read as the type its tag says
            let v = unsafe { &p.Anonymous.Anonymous };
            if v.vt == VT_UI8 {
                r.duration_ms = (unsafe { v.Anonymous.uhVal } / 10_000) as i64;
            }
        }
        Some(r)
    }

    /// The decoded pictures' sizes, from the video's current type.
    fn video_type(&mut self) -> Option<()> {
        let t = unsafe { self.reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32) }.ok()?;
        let packed = unsafe { t.GetUINT64(&MF_MT_FRAME_SIZE) }.ok()?;
        self.coded = ((packed >> 32) as u32, packed as u32);
        self.shown = aperture(&t).unwrap_or(self.coded);
        Some(())
    }

    /// The picture's size as shown (turned upright).
    fn size(&self) -> (u32, u32) {
        if !self.video {
            (0, 0)
        } else if self.rotation == 90 || self.rotation == 270 {
            (self.shown.1, self.shown.0)
        } else {
            self.shown
        }
    }

    /// Whether the reader's stream `index` is the video.
    fn is_video(&mut self, index: u32) -> bool {
        let i = index as usize;
        if self.kinds.len() <= i {
            self.kinds.resize(i + 1, None);
        }
        *self.kinds[i].get_or_insert_with(|| {
            unsafe { self.reader.GetCurrentMediaType(index) }
                .and_then(|t| unsafe { t.GetGUID(&MF_MT_MAJOR_TYPE) })
                .is_ok_and(|g| g == MFMediaType_Video)
        })
    }

    /// Reading on from `hns` into the file (from the keyframe before it).
    fn seek(&self, hns: i64) {
        let mut at = PROPVARIANT::default();
        // SAFETY: a plain 64-bit value, tagged as one
        unsafe {
            let v = &mut at.Anonymous.Anonymous;
            v.vt = VT_I8;
            v.Anonymous.hVal = hns;
            let _ = self.reader.SetCurrentPosition(&GUID::zeroed(), &at);
        }
    }
}

/// Waits until `due` (`false`: stopped meanwhile).
fn wait_until(due: Instant, stop: &AtomicBool) -> bool {
    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let now = Instant::now();
        if now >= due {
            return true;
        }
        std::thread::sleep((due - now).min(Duration::from_millis(20)));
    }
}

fn run(job: Job, opened: mpsc::Sender<Option<Opened>>) {
    let Some(_mf) = Mf::start() else {
        let _ = opened.send(None);
        return;
    };
    let mut gpu = Gpu::new();
    let Some(mut r) = Reader::open(&job.path, gpu.as_ref(), job.rate) else {
        let _ = opened.send(None);
        return;
    };
    if opened.send(Some((r.size(), r.duration_ms))).is_err() {
        return;
    }
    let mut copier = Copier::new();
    let mut stereo = Vec::new();
    // where the clock starts: the file's time `base` plays at the wall's `clock`
    let mut base = job.from_ms.max(0) * 10_000;
    if base > 0 {
        r.seek(base);
    }
    let mut clock = Instant::now();
    let (mut video_done, mut audio_done) = (!r.video, !r.audio);
    while !job.stop.load(Ordering::Relaxed) {
        let (mut index, mut flags, mut at) = (0u32, 0u32, 0i64);
        let mut sample = None;
        let read = unsafe {
            r.reader.ReadSample(
                MF_SOURCE_READER_ANY_STREAM.0 as u32,
                0,
                Some(&mut index),
                Some(&mut flags),
                Some(&mut at),
                Some(&mut sample),
            )
        };
        if let Err(e) = read {
            tracing::warn!(%e, id = %job.id, "media: Media Foundation stopped reading");
            break;
        }
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            break;
        }
        let video = r.is_video(index);
        if video && flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
            let _ = r.video_type();
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            if video { video_done = true } else { audio_done = true }
            if video_done && audio_done {
                if !job.looping {
                    break;
                }
                // round again from the start
                r.seek(0);
                (base, clock) = (0, Instant::now());
                (video_done, audio_done) = (!r.video, !r.audio);
            }
            continue;
        }
        let Some(sample) = sample else { continue };
        // decoded from the keyframe before where it plays from: not shown, not heard
        if at < base {
            continue;
        }
        let due = clock + Duration::from_nanos((at - base) as u64 * 100);
        if video {
            if Instant::now() > due + LATE {
                continue;
            }
            let picture = match gpu.as_mut() {
                Some(g) => on_gpu(&sample, g, r.shown, r.rotation, &mut copier),
                None => None,
            }
            .or_else(|| in_memory(&sample, r.coded, r.shown, &mut copier));
            if !wait_until(due, &job.stop) {
                break;
            }
            if let Some(buffer) = picture {
                crate::frames::publish(job.id, buffer);
            }
        } else {
            let Ok(buffer) = (unsafe { sample.ConvertToContiguousBuffer() }) else { continue };
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            if unsafe { buffer.Lock(&mut data, None, Some(&mut len)) }.is_err() {
                continue;
            }
            // SAFETY: the locked buffer holds `len` bytes of float samples
            let samples = unsafe { std::slice::from_raw_parts(data as *const f32, len as usize / 4) };
            let ch = r.channels as usize;
            stereo.clear();
            for frame in samples.chunks_exact(ch) {
                stereo.push(frame[0]);
                stereo.push(if ch > 1 { frame[1] } else { frame[0] });
            }
            let _ = unsafe { buffer.Unlock() };
            if !wait_until(due, &job.stop) {
                break;
            }
            if let Some(to) = &job.sound_to {
                to(&stereo, r.rate);
            } else if let Some(slot) = crate::audio_host::slot_of(job.id) {
                slot.push(&stereo, 2, r.rate);
            }
        }
    }
    ended(&job.stop, &job.control);
}
