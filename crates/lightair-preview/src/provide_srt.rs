//! Another computer's picture and sound over SRT ("From another computer": a media source with a
//! `link_port`, listening for MPEG-TS from another Lightair or OBS) as a picture provider, with
//! `LIGHTAIR_ENGINE=own`.
//!
//! - **Received only while shown.** Once a canvas's compositor shows the source, this provider
//!   listens on its port (SRT, the other computer calling) on a thread of its own; a second after
//!   no compositor shows it, it stops listening, and nothing listens until it's shown again (the
//!   other computer keeps calling until then).
//! - **The port.** While the switch is on, the reconciler gives libobs's media source for the
//!   link no address (`bridge::link_received_by_compositor`), so libobs never listens on the port
//!   this provider listens on. The document keeps the address.
//! - **Pictures.** The stream is split into access units as it arrives (the same MPEG-TS
//!   demuxer as the phone cameras') and decoded by VideoToolbox into BGRA IOSurfaces, shown the
//!   moment they're decoded, after a reorder of a few frames when the stream has B-frames. Each
//!   picture is a buffer of the decoder's own, which it doesn't reuse while it's held.
//! - **Sound.** AAC decoded by AudioToolbox is handed to libobs's media source for the link as
//!   its sound, so it reaches the mix, its meters and its track as before.
//! - **What libobs shows** of the source (thumbnails, Studio mode) is no longer the stream's
//!   picture while the switch is on: libobs receives nothing for it.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use futures::StreamExt;
use lightair_model::{Id, Source};
use lightair_obs::obs::SourceRef;
use lightair_obs::phone_camera::{Decoder, Pictures};
use lightair_obs::{bridge, sys};
use lightair_phone_camera::ts::{Codec, Demuxer, Kind, Unit};
use objc2_core_foundation::Type;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetIOSurface, CVPixelBufferGetPixelFormatType, kCVPixelFormatType_32BGRA,
};
use parking_lot::Mutex;
use srt_protocol::options::KeySize;
use srt_protocol::settings::{KeySettings, Passphrase};
use srt_tokio::SrtListener;
use tokio::sync::oneshot;

use crate::providers::{Frame, Provider};

/// How long listening goes on once no compositor shows the source.
const LINGER: Duration = Duration::from_secs(1);
/// SRT's latency when the address doesn't give one.
const LATENCY: Duration = Duration::from_millis(200);
/// How long an earlier receiver of the link may take to let go of the port.
const BIND_FOR: Duration = Duration::from_secs(3);
/// Access units waiting to be decoded, at most (more are dropped: the decoder is behind).
const QUEUE: usize = 256;
/// The most frames held to put a stream with B-frames in order.
const MOST_HELD: usize = 4;

/// How a link listens: its port and what its address says.
#[derive(Debug, Clone, PartialEq)]
struct Plan {
    port: u16,
    latency: Duration,
    passphrase: Option<String>,
}

impl Plan {
    /// The link's plan from the source's settings: `link_port`, and from `input`
    /// (`srt://0.0.0.0:<port>?mode=listener&latency=<µs>&passphrase=…`) its latency and passphrase.
    fn of(source: &Source) -> Option<Plan> {
        let port = u16::try_from(source.settings["link_port"].as_u64()?).ok()?;
        let input = source.settings["input"].as_str().unwrap_or_default();
        let query = input.split_once('?').map_or("", |(_, q)| q);
        let mut plan = Plan { port, latency: LATENCY, passphrase: None };
        for (key, value) in query.split('&').filter_map(|kv| kv.split_once('=')) {
            match key {
                "latency" => {
                    if let Ok(us) = value.parse::<u64>() {
                        plan.latency = Duration::from_micros(us);
                    }
                }
                "passphrase" => plan.passphrase = Some(unescape(value)).filter(|p| !p.is_empty()),
                _ => {}
            }
        }
        Some(plan)
    }
}

/// A URL query value with its `%XX` escapes (and `+`) turned back.
fn unescape(s: &str) -> String {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let escaped = (b[i] == b'%' && i + 2 < b.len()).then(|| Some(hex(b[i + 1])? << 4 | hex(b[i + 2])?)).flatten();
        match (escaped, b[i]) {
            (Some(v), _) => {
                out.push(v);
                i += 3;
            }
            (None, b'+') => {
                out.push(b' ');
                i += 1;
            }
            (None, c) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------- pictures

/// A picture handed to the compositor and its serial.
struct Shown(bridge::Buffer, u64);

// SAFETY: CoreVideo's buffers are reference counted thread-safely, and a decoded picture isn't
// written again while it's held.
unsafe impl Send for Shown {}

/// Pictures in decoding order put back in presentation order: each is held until enough later
/// ones have come. How many is learnt from the stream (none for a stream without B-frames): a
/// picture that comes after a later one was shown is dropped, and one more is held from then on.
#[derive(Default)]
struct Reorder {
    held: Vec<(u64, bridge::Buffer)>,
    depth: usize,
    /// The PTS last handed on.
    last: Option<u64>,
}

// SAFETY: as `Shown`.
unsafe impl Send for Reorder {}

impl Reorder {
    /// A decoded picture and its PTS: the pictures now due, in order.
    fn push(&mut self, pts: Option<u64>, picture: bridge::Buffer) -> Vec<bridge::Buffer> {
        let Some(pts) = pts else { return vec![picture] };
        if let Some(last) = self.last {
            if pts + 90_000 < last {
                // more than a second back: a new stream (or the clock wrapped), so start again
                self.held.clear();
                self.last = None;
            } else if pts <= last {
                self.depth = (self.depth + 1).min(MOST_HELD);
                return vec![];
            }
        }
        self.held.push((pts, picture));
        let mut due = vec![];
        while self.held.len() > self.depth {
            let Some(i) = self.held.iter().enumerate().min_by_key(|(_, (p, _))| *p).map(|(i, _)| i) else { break };
            let (p, picture) = self.held.swap_remove(i);
            self.last = Some(p);
            due.push(picture);
        }
        due
    }
}

/// A link's newest picture, written by its decoder and read by the compositor.
#[derive(Default)]
struct Slot {
    newest: Mutex<Option<Shown>>,
    serial: AtomicU64,
    reorder: Mutex<Reorder>,
}

impl Slot {
    /// A picture from the decoder (BGRA, IOSurface-backed), shown once it's due.
    fn decoded(&self, image: &CVPixelBuffer, pts: Option<u64>) {
        if CVPixelBufferGetPixelFormatType(image) != kCVPixelFormatType_32BGRA
            || CVPixelBufferGetIOSurface(Some(image)).is_none()
        {
            return;
        }
        let due = self.reorder.lock().push(pts, image.retain());
        // (only the newest due is shown: a compositor draws one picture a frame)
        if let Some(picture) = due.into_iter().last() {
            let serial = self.serial.fetch_add(1, Ordering::AcqRel) + 1;
            let old = self.newest.lock().replace(Shown(picture, serial));
            drop(old);
        }
    }
}

// ---------------------------------------------------------------- libobs's media source

/// The libobs media source of document source `id` (named by the id).
fn libobs_source(id: Id) -> Option<SourceRef> {
    let name = CString::new(id.to_string()).ok()?;
    // SAFETY: the name is a valid C string; libobs gives a new reference (or null)
    unsafe { SourceRef::from_owned(sys::obs_get_source_by_name(name.as_ptr())) }
}

// ---------------------------------------------------------------- receiving

/// Receives a link until `stopped` (its sender dropped): binds its port (retrying for a moment
/// while an earlier receiver of the link lets go of it), listens for SRT, splits what arrives into access units and has them decoded on a thread of
/// their own.
fn receive(id: Id, plan: Plan, slot: Arc<Slot>, mut stopped: oneshot::Receiver<()>) {
    let Some(source) = libobs_source(id) else {
        tracing::info!(%id, "another computer: no media source to receive for");
        return;
    };
    let started = Instant::now();
    let socket = loop {
        match std::net::UdpSocket::bind(("0.0.0.0", plan.port)) {
            Ok(s) => break s,
            Err(e) if started.elapsed() >= BIND_FOR => {
                tracing::warn!(port = plan.port, %e, "another computer: couldn't listen on the port");
                return;
            }
            Err(_) => {
                if !matches!(stopped.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    if let Err(e) = socket.set_nonblocking(true) {
        tracing::warn!(%e, "another computer: the socket can't be used");
        return;
    }
    let (units, queued) = mpsc::sync_channel::<Unit>(QUEUE);
    let decoding = source.clone();
    let spawned =
        std::thread::Builder::new().name("lightair-srt-decode".into()).spawn(move || decode(decoding, slot, queued));
    if spawned.is_err() {
        tracing::warn!("another computer: couldn't start the decoder");
        return;
    }
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(%e, "another computer: couldn't start receiving");
            return;
        }
    };
    tracing::info!(%id, port = plan.port, "another computer: listening");
    rt.block_on(listen(socket, plan, units, stopped));
    tracing::info!(%id, "another computer: stopped listening");
}

/// Listens for SRT on `socket` until `stopped`. One stream at a time: the newest one that brings
/// data takes over (a handshake given up on can still be accepted, and never sends anything).
async fn listen(
    socket: std::net::UdpSocket,
    plan: Plan,
    units: mpsc::SyncSender<Unit>,
    stopped: oneshot::Receiver<()>,
) {
    let socket = match tokio::net::UdpSocket::from_std(socket) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%e, "another computer: the socket can't be used");
            return;
        }
    };
    let bound = SrtListener::builder().latency(plan.latency).socket(socket).bind(plan.port).await;
    let (mut listener, mut incoming) = match bound {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(port = plan.port, %e, "another computer: couldn't listen for SRT");
            return;
        }
    };
    // the stream that carries the picture now (by serial), and every stream's task by serial
    let current = Arc::new(AtomicU64::new(0));
    let tasks: Arc<Mutex<HashMap<u64, tokio::task::AbortHandle>>> = Default::default();
    let accept = async {
        let mut serial = 0u64;
        while let Some(request) = incoming.incoming().next().await {
            serial += 1;
            let me = serial;
            let peer = request.remote();
            let key = plan.passphrase.as_ref().and_then(|p| {
                let passphrase = Passphrase::try_from(p.clone()).ok()?;
                let key_size = match request.key_size() {
                    KeySize::Unspecified => KeySize::AES128,
                    k => k,
                };
                Some(KeySettings { key_size, passphrase })
            });
            let (units, current, tasks2) = (units.clone(), current.clone(), tasks.clone());
            let task = tokio::spawn(async move {
                let Ok(mut socket) = request.accept(key).await else {
                    tracing::info!(%peer, "another computer: a stream didn't start");
                    tasks2.lock().remove(&me);
                    return;
                };
                let mut demux = Demuxer::new();
                while let Some(Ok((_, data))) = socket.next().await {
                    if current.load(Ordering::Acquire) != me {
                        let old = current.swap(me, Ordering::AcqRel);
                        if let Some(old) = tasks2.lock().remove(&old) {
                            old.abort();
                        }
                        tracing::info!(%peer, "another computer: stream started");
                    }
                    for unit in demux.push(&data) {
                        // (the decoder behind: dropped; a keyframe comes again)
                        let _ = units.try_send(unit);
                    }
                }
                tracing::info!(%peer, "another computer: stream ended");
                tasks2.lock().remove(&me);
            });
            tasks.lock().insert(me, task.abort_handle());
        }
    };
    tokio::select! {
        _ = accept => {}
        _ = stopped => {}
    }
    for (_, t) in tasks.lock().drain() {
        t.abort();
    }
    listener.close().await;
}

/// Decodes a link's access units until receiving stops: pictures into `slot`, sound into the
/// link's libobs media source. Once a second, libobs's media source is kept from listening.
fn decode(source: SourceRef, slot: Arc<Slot>, units: mpsc::Receiver<Unit>) {
    let into = slot;
    let pictures: Pictures = Arc::new(move |image, pts| into.decoded(image, pts));
    let mut decoder = Decoder::new(true, pictures);
    loop {
        match units.recv_timeout(Duration::from_secs(1)) {
            Ok(unit) => match unit.kind {
                Kind::Video(codec) => decoder.video(codec == Codec::Hevc, &unit.data, unit.pts, unit.dts),
                // SAFETY: the source is held for the whole loop
                Kind::Audio => unsafe { decoder.sound(&unit.data, source.as_ptr()) },
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

// ---------------------------------------------------------------- the provider

/// A link being received.
struct Link {
    plan: Plan,
    slot: Arc<Slot>,
    /// Dropped to stop receiving (the receiving thread ends by itself).
    _stop: oneshot::Sender<()>,
    unwanted_since: Option<Instant>,
}

#[derive(Default)]
struct State {
    /// The links each canvas shows now.
    shown: HashMap<Id, HashSet<Id>>,
    links: HashMap<Id, Link>,
}

#[derive(Default)]
pub(crate) struct Srt {
    state: Mutex<State>,
}

impl Provider for Srt {
    fn handles(&self, source: &Source) -> bool {
        bridge::link_received_by_compositor(source)
    }

    fn want(&self, canvas: Id, shown: &[&Source]) {
        let mut state = self.state.lock();
        if shown.is_empty() {
            state.shown.remove(&canvas);
        } else {
            state.shown.insert(canvas, shown.iter().map(|s| s.id).collect());
        }
        let now = Instant::now();
        for source in shown {
            let Some(plan) = Plan::of(source) else { continue };
            // a link whose address changed listens anew
            if state.links.get(&source.id).is_some_and(|l| l.plan != plan) {
                state.links.remove(&source.id);
            }
            if let Some(link) = state.links.get_mut(&source.id) {
                link.unwanted_since = None;
                continue;
            }
            let slot = Arc::new(Slot::default());
            let (stop, stopped) = oneshot::channel();
            let (id, receiving, into) = (source.id, plan.clone(), slot.clone());
            let spawned = std::thread::Builder::new()
                .name("lightair-srt-in".into())
                .spawn(move || receive(id, receiving, into, stopped));
            if spawned.is_err() {
                tracing::warn!("another computer: couldn't start receiving");
                continue;
            }
            state.links.insert(source.id, Link { plan, slot, _stop: stop, unwanted_since: None });
        }
        // a second after no compositor shows it, receiving stops
        let all: HashSet<Id> = state.shown.values().flatten().copied().collect();
        state.links.retain(|id, link| {
            if all.contains(id) {
                link.unwanted_since = None;
                return true;
            }
            let since = *link.unwanted_since.get_or_insert(now);
            now.duration_since(since) < LINGER
        });
    }

    fn frame(&self, source: Id) -> Option<Frame> {
        let slot = self.state.lock().links.get(&source)?.slot.clone();
        let newest = slot.newest.lock();
        let Shown(buffer, serial) = newest.as_ref()?;
        Some(Frame { buffer: buffer.clone(), serial: *serial })
    }
}
