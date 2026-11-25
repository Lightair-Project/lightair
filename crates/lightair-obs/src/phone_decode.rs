//! A phone camera's stream (or another computer's) decoded into libobs (macOS): the decoder is
//! `lightair_host::phone_decode`; here its pictures are copied into a private async source when
//! the compositor doesn't take them, and its sound goes to a track's feed in the mixer when one
//! takes it, else into a libobs source as that source's sound.

use std::ffi::c_void;
use std::sync::Arc;

use objc2_core_video::CVPixelBuffer;

pub use lightair_host::phone_decode::*;

use crate::phone_camera::{Feed, Unit, UnitKind};
use crate::sys;
use lightair_host::phone_units::bt709;

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferLockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferGetPixelFormatType(buffer: *mut c_void) -> u32;
    fn CVPixelBufferGetWidth(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetHeight(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetBaseAddress(buffer: *mut c_void) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRow(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(buffer: *mut c_void, plane: usize) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRowOfPlane(buffer: *mut c_void, plane: usize) -> usize;
}

/// One unit of AAC in ADTS decoded as libobs source `into`'s sound: into the mixer when a track
/// takes it (the source registered a feed by its document id), else output into `into`, timed
/// now.
///
/// # Safety
/// `into` must be a live libobs source.
pub unsafe fn sound_into(decoder: &mut Decoder, unit: &[u8], into: *mut sys::obs_source_t) {
    let slot = crate::obs::doc_id(into).and_then(crate::audio_host::slot_of);
    decoder.sound(unit, &mut |planes: &[&[f32]], rate: u32| {
        if slot.as_ref().is_some_and(|s| s.push_planar(planes, rate)) {
            return;
        }
        let Some(frames) = planes.first().map(|p| p.len()) else { return };
        let mut a = sys::obs_source_audio {
            frames: frames as u32,
            speakers: if planes.len() >= 2 { sys::SPEAKERS_STEREO } else { sys::SPEAKERS_MONO },
            format: sys::AUDIO_FORMAT_FLOAT_PLANAR,
            samples_per_sec: rate,
            timestamp: crate::clock::now_ns(),
            ..Default::default()
        };
        for (o, p) in a.data.iter_mut().zip(planes) {
            *o = p.as_ptr() as *const u8;
        }
        unsafe { sys::obs_source_output_audio(into, &a) };
    });
}

/// Copies a decoded picture into libobs's async source `frames` (libobs copies it again).
///
/// # Safety
/// `frames` must be a live async video source.
unsafe fn to_libobs(frames: *mut sys::obs_source_t, image: &CVPixelBuffer) {
    let image = image as *const CVPixelBuffer as *mut c_void;
    unsafe {
        CVPixelBufferLockBaseAddress(image, 1);
        let format = CVPixelBufferGetPixelFormatType(image);
        let (w, h) = (CVPixelBufferGetWidth(image) as u32, CVPixelBufferGetHeight(image) as u32);
        let mut f =
            sys::obs_source_frame { width: w, height: h, timestamp: crate::clock::now_ns(), ..Default::default() };
        let ok = match format {
            FOURCC_420V | FOURCC_420F => {
                let full = format == FOURCC_420F;
                let (m, lo, hi) = bt709(full);
                f.format = sys::VIDEO_FORMAT_NV12;
                f.full_range = full;
                f.color_matrix = m;
                f.color_range_min = lo;
                f.color_range_max = hi;
                for p in 0..2 {
                    f.data[p] = CVPixelBufferGetBaseAddressOfPlane(image, p) as *mut u8;
                    f.linesize[p] = CVPixelBufferGetBytesPerRowOfPlane(image, p) as u32;
                }
                true
            }
            FOURCC_BGRA => {
                f.format = sys::VIDEO_FORMAT_BGRA;
                f.data[0] = CVPixelBufferGetBaseAddress(image) as *mut u8;
                f.linesize[0] = CVPixelBufferGetBytesPerRow(image) as u32;
                true
            }
            _ => false,
        };
        if ok {
            // (libobs copies the picture)
            sys::obs_source_output_video(frames, &f);
        }
        CVPixelBufferUnlockBaseAddress(image, 1);
    }
}

/// Decodes a feed's units into `frames` (a private async source) until the feed lets go. With
/// `LIGHTAIR_ENGINE=own` pictures are BGRA IOSurfaces, and while Lightair's compositor takes the
/// feed's pictures (`Feed::take`) they go to it instead of being copied into libobs. The sound
/// goes into `frames` either way.
pub(crate) fn run(frames: crate::obs::ObsSource, feed: Arc<Feed>, units: crossbeam_channel::Receiver<Unit>) {
    let target = frames.as_ptr() as usize;
    let to = feed;
    let pictures: Pictures = Arc::new(move |image, pts| {
        if !to.offer(image) {
            // SAFETY: `frames` outlives the decoder, which holds this
            unsafe { to_libobs(target as *mut sys::obs_source_t, image) };
        }
        if let Some(pts) = pts {
            to.decoded(pts);
            to.shown_next(pts);
        }
    });
    let mut decoder = Decoder::new(crate::bridge::own(), pictures);
    for unit in units {
        match unit.kind {
            UnitKind::H264 | UnitKind::Hevc => decoder.video(unit.kind == UnitKind::Hevc, &unit.data, unit.pts, None),
            // SAFETY: `frames` is held for the whole loop
            UnitKind::Aac => unsafe { sound_into(&mut decoder, &unit.data, frames.as_ptr()) },
        }
    }
}
