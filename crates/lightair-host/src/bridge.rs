//! What Lightair's own compositor (`lightair-preview`) reads besides the document: the show's
//! state ([`crate::show`]), the pictures of sources something else draws, and each avatar's
//! posing.
//!
//! - **Pictures from elsewhere:** while libobs still draws some sources, it draws the ones the
//!   compositor wants ([`want`]) and hands their pictures over ([`picture`]) through the hooks
//!   it installs ([`set_elsewhere`]). With no hooks (no libobs) nothing is drawn elsewhere: the
//!   compositor draws what it can itself.
//! - **Avatars:** whatever poses an avatar (libobs's avatar source today) publishes its posing by
//!   document id ([`set_avatar_posing`]), and the compositor poses its own drawing from it.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Instant;

use lightair_model::Id;
use objc2_core_foundation::CFRetained;
use parking_lot::Mutex;

pub use crate::buffers::{Buffer, bgra_buffer, surface_of};
pub use crate::show::{
    Snapshot, Switch, clear_timer_texts, link_received_by_compositor, own, publish, set_switch, set_timer_text,
    snapshot, timer_text,
};
pub use crate::tracker::{FACE_POINTS, FaceTracker, RawFace, RawMask};

/// A source's picture: a BGRA, IOSurface-backed pixel buffer, colours multiplied by alpha.
pub struct Picture {
    pub buffer: Buffer,
    /// Drawn without the source's own filters elsewhere: its effects aren't in it.
    pub unfiltered: bool,
    /// Says which picture it is: a different value is a new picture (buffers are reused).
    pub serial: u64,
}

impl Picture {
    /// The buffer as the renderer takes it.
    pub fn pixel_buffer(&self) -> Option<lightair_render::texture::PixelBuffer> {
        let buffer = self.buffer.clone();
        // SAFETY: the reference made by `clone` is given up to the pixel buffer, which releases it.
        unsafe { lightair_render::texture::PixelBuffer::from_raw(CFRetained::into_raw(buffer).as_ptr().cast()) }
    }
}

/// Where pictures of sources the compositor doesn't make come from (libobs, while it's there).
pub trait Elsewhere: Send + Sync {
    /// The compositor for `canvas` wants pictures of `sources` from now on (replacing what it
    /// wanted; empty when it stops); `true`: without the source's own filters.
    fn want(&self, canvas: Id, sources: HashMap<Id, bool>);
    /// The newest finished picture of `source`, if it's wanted and has been drawn.
    fn picture(&self, source: Id) -> Option<Picture>;
    /// Sound of `source` the mixer doesn't take (planar, `rate`), for whatever plays it there.
    fn sound(&self, _source: Id, _planes: &[&[f32]], _rate: u32) {}
}

static ELSEWHERE: OnceLock<Box<dyn Elsewhere>> = OnceLock::new();

/// Installs the hooks pictures come from (once; later calls are ignored).
pub fn set_elsewhere(hooks: Box<dyn Elsewhere>) {
    let _ = ELSEWHERE.set(hooks);
}

/// Whether something else draws sources for the compositor (libobs). Without it the compositor
/// draws every source it can make itself, with the effects it can draw.
pub fn elsewhere() -> bool {
    ELSEWHERE.get().is_some()
}

/// The compositor for `canvas` wants pictures of `sources` (see [`Elsewhere::want`]).
pub fn want(canvas: Id, sources: HashMap<Id, bool>) {
    if let Some(e) = ELSEWHERE.get() {
        e.want(canvas, sources);
    }
}

/// The newest picture of `source` drawn elsewhere; `None` with nothing drawing it. Any thread.
pub fn picture(source: Id) -> Option<Picture> {
    ELSEWHERE.get()?.picture(source)
}

/// Sound a source the compositor receives itself (another computer's link) decoded: into the
/// mixer when a track takes the source, else to the hooks (libobs's source of it).
pub fn sound(source: Id, planes: &[&[f32]], rate: u32) {
    if crate::audio_host::slot_of(source).is_some_and(|s| s.push_planar(planes, rate)) {
        return;
    }
    if let Some(e) = ELSEWHERE.get() {
        e.sound(source, planes, rate);
    }
}

// ---------------------------------------------------------------- avatars

/// What posed an avatar source on its last tick, so the compositor can pose its own drawing of
/// the same avatar. Whatever poses it keeps the camera, the trackers, the calibration, the
/// co-host's face and mood, the mic level and push to talk; the compositor only reads this.
#[derive(Clone, Debug)]
pub struct AvatarPosing {
    /// The face seen, calibrated and mirrored as the avatar's settings say, with the co-host's
    /// face merged in when this is its avatar; `None`: none seen (the avatar settles).
    pub face: Option<lightair_avatar::Face>,
    /// The body seen (a VRM's body tracking; `None` for flat avatars or with the body off).
    pub body: Option<lightair_avatar::body::BodyFrame>,
    /// The co-host's mood over the avatar's own expression, and its weight.
    pub mood: Option<(&'static str, f32)>,
    /// A PNGtuber's mic track level (dBFS; `-inf` for silence or no mic).
    pub mic_db: f32,
    /// A PNGtuber's push to talk is held.
    pub push_to_talk_held: bool,
    /// With `show_tracking`: the face's points in the trackers' picture (0..1 across and down).
    pub view_face: Vec<[f32; 2]>,
    /// The anchors something is pinned to: each one's place in the avatar's picture at rest and
    /// now.
    pub pins: Vec<(String, crate::avatar_pins::Spot, crate::avatar_pins::Spot)>,
    /// The avatar's picture size the pins' places are measured in.
    pub size: (u32, u32),
    /// When it was published.
    pub at: Instant,
}

/// The trackers' picture an avatar source last handed its trackers (with `show_tracking`).
#[derive(Clone, Debug)]
pub struct TrackingPicture {
    /// BGRA, rows `width * 4` bytes apart, top row first.
    pub bgra: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

#[derive(Default)]
struct AvatarFeed {
    posing: Option<Arc<AvatarPosing>>,
    view: Option<TrackingPicture>,
}

/// Each avatar source's last posing and tracking picture, by document id.
static AVATARS: LazyLock<Mutex<HashMap<Id, AvatarFeed>>> = LazyLock::new(Default::default);

/// The avatar source `source` was posed with `posing` (its tick).
pub fn set_avatar_posing(source: Id, posing: AvatarPosing) {
    AVATARS.lock().entry(source).or_default().posing = Some(Arc::new(posing));
}

/// The avatar source `source` handed its trackers this picture (`show_tracking` on).
pub fn set_avatar_view(source: Id, bgra: Arc<Vec<u8>>, size: (u32, u32)) {
    let view = TrackingPicture { bgra, width: size.0, height: size.1 };
    AVATARS.lock().entry(source).or_default().view = Some(view);
}

/// The avatar source `source` is gone.
pub fn forget_avatar(source: Id) {
    // (freed outside the lock)
    let gone = AVATARS.lock().remove(&source);
    drop(gone);
}

/// The avatar source `source`'s last posing, and its trackers' picture when it shows one;
/// `None` before it's been posed. Any thread.
pub fn avatar_posing(source: Id) -> Option<(Arc<AvatarPosing>, Option<TrackingPicture>)> {
    let feeds = AVATARS.lock();
    let feed = feeds.get(&source)?;
    Some((feed.posing.clone()?, feed.view.clone()))
}

// ---------------------------------------------------------------- captures the compositor runs

/// The newest picture of each document source whose capture Lightair's compositor runs itself.
struct Taken(Option<HashMap<Id, Buffer>>);

// SAFETY: CoreVideo's buffers are reference counted thread-safely, and a captured picture isn't
// written again once it's handed over.
unsafe impl Send for Taken {}
unsafe impl Sync for Taken {}

static TAKEN: Mutex<Taken> = Mutex::new(Taken(None));

/// The compositor captures document source `source` itself, and `buffer` (BGRA, IOSurface-backed)
/// is its newest picture (libobs's wrapper of the source lets its own capture go and draws this
/// instead). Any thread (a capture's own queue: the lock is held only to swap a reference).
pub fn publish_capture(source: Id, buffer: &Buffer) {
    TAKEN.lock().0.get_or_insert_with(HashMap::new).insert(source, buffer.clone());
    // a Studio take recording it
    crate::studio_take::frame(source, buffer);
}

/// The compositor no longer captures `source`.
pub fn release_capture(source: Id) {
    // (the buffer is let go outside the lock)
    let gone = TAKEN.lock().0.as_mut().and_then(|t| t.remove(&source));
    drop(gone);
}

/// The compositor's newest picture of `source`, while it captures it.
pub fn captured(source: Id) -> Option<Buffer> {
    TAKEN.lock().0.as_ref()?.get(&source).cloned()
}
