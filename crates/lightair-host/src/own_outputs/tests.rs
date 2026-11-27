//! Lightair's own outputs: keyframes on the program clock (whole intervals from the first), every
//! canvas's frames on the clock's own grid (so a multitrack stream's tracks from different canvases carry the same times, start
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

/// A stream's keyframes fall every interval on the program clock counted from its first one
/// (0.5 s, 2.5 s, 4.5 s, …), so the first interval is as long as the others: a platform checking
/// the interval sees the same one from the start.
#[test]
fn keyframes_come_every_interval_from_the_first() {
    let gop = ready(2, &[1]);
    let at = keys(&gop, 0, &frames(60, 0.5, 9.0));
    assert_eq!(at, vec![S / 2, 5 * S / 2, 9 * S / 2, 13 * S / 2, 17 * S / 2], "{at:?}");
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
    assert_eq!(a, vec![S, 3 * S, 5 * S]);
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

// ---------------------------------------------------------------- a stream's own keyframe beat

/// The keyframes a stream's own beat (no multitrack stream) makes on frames at `times`.
fn own_keys(keyint_s: i64, times: impl IntoIterator<Item = i64>) -> Vec<i64> {
    let mut next = i64::MIN;
    let mut out = Vec::new();
    for pts in times {
        if let Some(after) = own_key(next, pts, keyint_s * S) {
            assert!(after > pts, "the next keyframe is due after this one ({after} after {pts})");
            next = after;
            out.push(pts);
        }
    }
    out
}

/// The first keyframe comes on the first frame, and the next a whole interval after it (not on
/// the program clock's 2 s grid, which would make the first interval short).
#[test]
fn a_streams_first_keyframe_interval_is_as_long_as_the_others() {
    let at = own_keys(2, frames(60, 0.5, 9.0).into_iter().map(|(_, pts)| pts));
    assert_eq!(at, vec![S / 2, 5 * S / 2, 9 * S / 2, 13 * S / 2, 17 * S / 2], "{at:?}");
    for w in at.windows(2) {
        assert_eq!(w[1] - w[0], 2 * S, "every interval is the same: {at:?}");
    }
}

/// A frame just short of the due time isn't a keyframe; the first one at or past it is.
#[test]
fn a_keyframe_is_due_from_its_time_not_before() {
    assert_eq!(own_key(i64::MIN, 0, 2 * S), Some(2 * S), "the first frame is a keyframe");
    assert_eq!(own_key(2 * S, 2 * S - 1, 2 * S), None);
    assert_eq!(own_key(2 * S, 2 * S, 2 * S), Some(4 * S));
    // a frame a little late: the beat stays on the first keyframe's grid
    assert_eq!(own_key(2 * S, 2 * S + 7_000_000, 2 * S), Some(4 * S));
}

/// When no frame comes for a while (a stall, a dropped stretch), the next frame is a keyframe
/// and the beat goes on from the first keyframe's grid, skipping the intervals nothing came in.
#[test]
fn skipped_keyframes_keep_the_first_ones_grid() {
    // frames from 0.5 s to 3 s, then nothing until 9.3 s
    let times = frames(30, 0.5, 3.0).into_iter().chain(frames(30, 9.3, 13.0)).map(|(_, pts)| pts);
    let at = own_keys(2, times);
    assert_eq!(at, vec![S / 2, 5 * S / 2, 93 * S / 10, 105 * S / 10, 125 * S / 10], "{at:?}");
    // the one after the stall is due on the grid (10.5 s), not 2 s after the late keyframe
    assert_eq!(own_key(9 * S / 2, 93 * S / 10, 2 * S), Some(105 * S / 10));
    // several whole intervals skipped
    assert_eq!(own_key(2 * S, 100 * S, 2 * S), Some(102 * S));
}

/// Frames before the program clock's zero (negative times) beat the same way.
#[test]
fn an_early_stream_beats_from_a_negative_time() {
    let at = own_keys(2, frames(30, -1.5, 3.0).into_iter().map(|(_, pts)| pts));
    assert_eq!(at, vec![-3 * S / 2, S / 2, 5 * S / 2], "{at:?}");
    assert_eq!(own_key(i64::MIN, -7 * S / 10, 2 * S), Some(13 * S / 10));
    // a frame far past a negative due time: whole intervals skipped, rounding down
    assert_eq!(own_key(-S, 4 * S + S / 2, 2 * S), Some(5 * S));
}

/// A multitrack stream's first interval is as long as the others too: whole intervals from the
/// stream's first keyframe, on every canvas.
#[test]
fn a_multitrack_streams_first_interval_is_as_long_as_the_others() {
    let gop = ready(2, &[1, 2]);
    let all = frames(60, 0.5, 7.0);
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for f in &all {
        a.extend(keys(&gop, 0, std::slice::from_ref(f)));
        b.extend(keys(&gop, 1, std::slice::from_ref(f)));
    }
    assert_eq!(a, vec![S / 2, 5 * S / 2, 9 * S / 2, 13 * S / 2], "{a:?}");
    assert_eq!(a, b, "every canvas keys on the same moments");
}

/// A multitrack stream that starts before the clock's zero rounds the same way (`div_euclid`).
#[test]
fn a_multitrack_stream_beats_from_a_negative_start() {
    let gop = ready(2, &[1]);
    let at = keys(&gop, 0, &frames(30, -1.5, 3.0));
    assert_eq!(at, vec![-3 * S / 2, S / 2, 5 * S / 2], "{at:?}");
}

/// After frames stop for a while, the next is a keyframe and the beat goes on from the stream's
/// first keyframe's grid; an asked-for keyframe doesn't move the grid either.
#[test]
fn a_multitrack_streams_grid_survives_a_stall_and_a_request() {
    let gop = ready(2, &[1]);
    let times: Vec<(i64, i64)> = frames(60, 0.0, 1.0).into_iter().chain(frames(60, 5.3, 7.0)).collect();
    let at = keys(&gop, 0, &times);
    assert_eq!(at, vec![0, 53 * S / 10, 6 * S], "{at:?}");

    let gop = ready(2, &[1]);
    let all = frames(60, 0.0, 5.0);
    let mut at = keys(&gop, 0, &all[..60]);
    gop.asked.fetch_add(1, Ordering::AcqRel);
    at.extend(keys(&gop, 0, &all[60..]));
    // asked at 1 s: answered a little ahead (1.05 s), then 2 s, 4 s as before
    assert_eq!(at, vec![0, S + ASK_AHEAD_NS, 2 * S, 4 * S], "{at:?}");
}
