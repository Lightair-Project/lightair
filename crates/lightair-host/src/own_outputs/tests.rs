//! Lightair's own outputs: keyframes on the program clock, every canvas's frames on the clock's
//! own grid (so a multitrack stream's tracks from different canvases carry the same times, start
//! on one keyframe and answer a keyframe request together), and the platform's plan read into
//! encoder settings (CRF as constant quality, spatial AQ, the point scaler).

use super::*;

const S: i64 = 1_000_000_000;

/// A canvas's frames as the clock's grid places them: (slot, pts) for each frame at `fps`
/// from `from` to `to` seconds.
fn frames(fps: i64, from: f64, to: f64) -> Vec<(i64, i64)> {
    let (first, last) = ((from * fps as f64).round() as i64, (to * fps as f64).round() as i64);
    (first..last).map(|slot| (slot, slot * S / fps)).collect()
}

/// The times of the frames `canvas` makes keyframes on.
fn keys(gop: &Gop, canvas: usize, frames: &[(i64, i64)]) -> Vec<i64> {
    frames
        .iter()
        .filter(|(slot, pts)| matches!(gop.beat(canvas, *slot, *pts), Beat::Key))
        .map(|(_, pts)| *pts)
        .collect()
}

fn ready(keyint_s: u32, steps: &[i64]) -> Gop {
    let gop = Gop::new(keyint_s, steps);
    gop.ready.store(true, Ordering::Release);
    gop
}

/// A stream's keyframes fall every interval on the program clock (2 s, 4 s, …), not every
/// interval counted from when the stream started: then the renditions and the platform's
/// segment boundaries line up.
#[test]
fn keyframes_come_every_interval_on_the_program_clock() {
    let gop = ready(2, &[1]);
    let at = keys(&gop, 0, &frames(60, 0.5, 9.0));
    assert_eq!(at, vec![S / 2, 2 * S, 4 * S, 6 * S, 8 * S], "{at:?}");
}

/// A track taking every other frame of its canvas (30 fps from 60) keys on the same moments as
/// the full-rate one: each moment is one keyframe on every track.
#[test]
fn tracks_at_different_rates_key_on_the_same_moments() {
    let gop = ready(2, &[1, 2]);
    let (fast, slow) = (frames(60, 1.0, 7.0), frames(60, 1.0, 7.0));
    let mut a = Vec::new();
    let mut b = Vec::new();
    for (f, s) in fast.iter().zip(&slow) {
        a.extend(keys(&gop, 0, std::slice::from_ref(f)));
        b.extend(keys(&gop, 1, std::slice::from_ref(s)));
    }
    assert_eq!(a, b, "the full-rate track keyed at {a:?}, the half-rate one at {b:?}");
    assert_eq!(a, vec![S, 2 * S, 4 * S, 6 * S]);
}

/// A multitrack stream starts on one keyframe: a canvas whose frames run behind the one that
/// started the stream waits for that moment rather than starting earlier (a short first GOP on
/// one track, misaligned renditions).
#[test]
fn a_multitrack_stream_starts_on_one_keyframe_on_every_canvas() {
    let gop = ready(2, &[1, 1]);
    // canvas 0 is first to a frame, at 3.0 s; canvas 1's thread is a few frames behind
    let started = keys(&gop, 0, &frames(30, 3.0, 3.1));
    let behind = keys(&gop, 1, &frames(30, 2.9, 3.1));
    assert_eq!(started, vec![3 * S]);
    assert_eq!(behind, vec![3 * S], "the late canvas starts at the stream's start, not before it");
}

/// A keyframe the connection asks for comes at one time on every canvas, a little ahead so the
/// canvas that's behind still has that frame to come.
#[test]
fn a_keyframe_asked_for_comes_at_one_time_on_every_canvas() {
    let gop = ready(10, &[1, 1]);
    // canvas 0's thread runs three frames ahead of canvas 1's
    let ahead = 3;
    let all = frames(60, 1.0, 3.0);
    for (slot, pts) in &all[..60] {
        gop.beat(0, *slot, *pts);
    }
    for (slot, pts) in &all[..60 - ahead] {
        gop.beat(1, *slot, *pts);
    }
    gop.asked.fetch_add(1, Ordering::AcqRel);
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for k in 60..all.len() {
        a.extend(keys(&gop, 0, &all[k..=k]));
        b.extend(keys(&gop, 1, &all[k - ahead..=k - ahead]));
    }
    assert_eq!(a.len(), 1, "one keyframe answers the request: {a:?}");
    assert_eq!(a, b, "both canvases answer at the same moment");
}

/// What a canvas's thread hands its encoders: (place on the clock's grid, pts) of each new frame.
#[derive(Default)]
struct Pushed(parking_lot::Mutex<Vec<(i64, i64)>>);

impl Consumer for Pushed {
    fn push(&self, frame: Option<&Buffer>, slot: i64, pts: i64) {
        if frame.is_some() {
            self.0.lock().push((slot, pts));
        }
    }
}

/// Runs a canvas's thread on frames compositor-stamped at `times` (ns), giving what its
/// encoders got.
fn run(times: &[i64], fps: (u32, u32), picture: &Buffer) -> Vec<(i64, i64)> {
    let (tx, rx) = unbounded();
    let pushed = Arc::new(Pushed::default());
    let pipes: Consumers = Arc::new(Mutex::new(vec![pushed.clone() as Arc<dyn Consumer>]));
    let thread = std::thread::spawn(move || run_canvas(rx, pipes, fps, Arc::default()));
    for t in times {
        tx.send((Frame(picture.clone()), *t)).unwrap();
    }
    drop(tx);
    thread.join().unwrap();
    pushed.0.lock().clone()
}

/// Two canvases at one rate whose compositors stamp the same moments a few milliseconds apart
/// give their frames the very same times, so a multitrack stream's tracks from different
/// canvases line up (and every time is on the clock's frame grid, not the first frame's).
#[test]
fn canvases_of_one_rate_give_one_moment_one_time() {
    let Some(picture) = crate::buffers::bgra_buffer(16, 16) else {
        eprintln!("no GPU picture here: skipped");
        return;
    };
    let frame = S / 30;
    let base = 12_345 * S + 7_000_000;
    let a: Vec<i64> = (0..30).map(|k| base + k * frame + 1_000_000).collect();
    let b: Vec<i64> = (0..30).map(|k| base + k * frame + 4_000_000).collect();
    let (a, b) = (run(&a, (30, 1), &picture), run(&b, (30, 1), &picture));
    assert_eq!(a, b, "one moment, one time on both canvases");
    for (place, pts) in &a {
        assert_eq!(*pts, place * S / 30, "frame {place} is on the clock's grid");
    }
    // NTSC rates too: 30000/1001 frames from two canvases, a frame apart in when they started
    let ntsc = S * 1001 / 30_000;
    let c: Vec<i64> = (0..20).map(|k| base + k * ntsc + 2_000_000).collect();
    let d: Vec<i64> = (1..20).map(|k| base + k * ntsc + 5_000_000).collect();
    let (c, d) = (run(&c, (30_000, 1001), &picture), run(&d, (30_000, 1001), &picture));
    assert_eq!(&c[1..], &d[..], "the canvas that started a frame later joins the same grid");
}

// ---------------------------------------------------------------- the plan's settings

fn plan(settings: serde_json::Value, scale: Option<&str>) -> (EncoderConfig, Tune) {
    let cfg = StreamConfig {
        id: Id::new(),
        server: "rtmp://localhost/app".into(),
        key: "key".into(),
        encoders: outputs::default_encoders(&[], 6000, true),
        reconnect: None,
        vod_mix: None,
        net: Default::default(),
    };
    let track = outputs::MultitrackVideo {
        encoder: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        width: 1280,
        height: 720,
        fps_divisor: 1,
        settings,
        scale: scale.map(str::to_string),
    };
    let (enc, _, tune) = track_encoder(&cfg, &track);
    (enc, tune)
}

/// The plan's CRF (and its kin: CQ, CQP, ICQ) is constant quality, not the bitrate: lower
/// quantisers are better pictures, and a plan's own `quality` (Apple's encoders' 0–100) is
/// taken as it is. A CBR plan stays at its bitrate.
#[test]
fn a_crf_plan_is_constant_quality() {
    use serde_json::json;
    let quality = |s: serde_json::Value| plan(s, None).1.quality;
    let (fine, coarse) =
        (quality(json!({"rate_control": "CRF", "crf": 18})), quality(json!({"rate_control": "crf", "crf": "30"})));
    assert!(fine.zip(coarse).is_some_and(|(f, c)| f > c), "CRF 18 is finer than CRF 30: {fine:?} {coarse:?}");
    assert!(quality(json!({"rate_control": "CQP", "cqp": 22})).is_some());
    assert!(quality(json!({"rate_control": "ICQ", "icq_quality": 25})).is_some());
    assert_eq!(quality(json!({"rate_control": "CRF", "quality": 55})), Some(55));
    let (enc, tune) = plan(json!({"rate_control": "CRF", "crf": 23, "bitrate": 350}), None);
    assert!(!enc.constant_bitrate, "a constant quality stream isn't held at its bitrate");
    assert!(tune.quality.is_some());

    let (enc, tune) = plan(json!({"rate_control": "CBR", "bitrate": 350}), None);
    assert!(enc.constant_bitrate && tune.quality.is_none());
    assert_eq!(enc.video_bitrate_kbps, 350);
}

/// Spatial adaptive quantisation as the plan says it, in any of its spellings; "auto" (or none
/// said) leaves it to the encoder.
#[test]
fn spatial_aq_follows_the_plan() {
    use serde_json::json;
    let aq = |s: serde_json::Value| plan(s, None).1.spatial_aq;
    assert_eq!(aq(json!({"spatial_aq_mode": "ENABLED"})), Some(true));
    assert_eq!(aq(json!({"spatial_aq_mode": 1})), Some(true));
    assert_eq!(aq(json!({"spatial_aq_mode": "disabled"})), Some(false));
    assert_eq!(aq(json!({"spatial_aq_mode": 0})), Some(false));
    assert_eq!(aq(json!({"spatial_aq_mode": "AUTO"})), None);
    assert_eq!(aq(json!({"spatial_aq_mode": -1})), None);
    assert_eq!(aq(json!({"psycho_aq": true})), Some(true));
    assert_eq!(aq(json!({"spatial_aq": "0"})), Some(false));
    assert_eq!(aq(json!({})), None);
}

/// The plan's point scaler picks pixels (fast); every other scaler averages them.
#[test]
fn only_the_point_scaler_scales_by_picking_pixels() {
    let fast = |scale: Option<&str>| plan(serde_json::json!({}), scale).1.fast_scale;
    assert!(fast(Some("OBS_SCALE_POINT")));
    assert!(!fast(Some("OBS_SCALE_BICUBIC")) && !fast(Some("OBS_SCALE_LANCZOS")) && !fast(None));
}
