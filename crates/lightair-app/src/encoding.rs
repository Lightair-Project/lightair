//! From settings to encoders (docs/11-outputs-streaming-recording.md, "Encoding"): which libobs
//! encoder a choice means on this machine, the size it encodes at, and what Automatic picks for
//! a destination.

use lightair_model::{Destination, Encoding, Canvas, Workspace, RecordingSettings, VideoEncoder};
use lightair_engine::outputs::{
    EncoderConfig, StreamProtocol, H264_ENCODERS, HEVC_ENCODERS, PRORES_ENCODERS, aac_encoder, first_available,
};

use crate::platforms;

/// The libobs encoder for a choice, among those this machine has; hardware H.264 (then x264)
/// when the choice isn't available.
pub fn encoder_id(available: &[String], choice: VideoEncoder) -> String {
    let h264 = || first_available(available, H264_ENCODERS).unwrap_or_else(|| "obs_x264".into());
    match choice {
        VideoEncoder::H264 => h264(),
        VideoEncoder::Hevc => first_available(available, HEVC_ENCODERS).unwrap_or_else(h264),
        VideoEncoder::ProRes => first_available(available, PRORES_ENCODERS).unwrap_or_else(h264),
        VideoEncoder::X264 => "obs_x264".into(),
    }
}

/// Whether this machine has an encoder for the choice.
pub fn available(available: &[String], choice: VideoEncoder) -> bool {
    let id = encoder_id(available, choice);
    match choice {
        VideoEncoder::H264 => true,
        VideoEncoder::Hevc => HEVC_ENCODERS.contains(&id.as_str()),
        VideoEncoder::ProRes => id.contains("prores"),
        VideoEncoder::X264 => available.iter().any(|e| e == "obs_x264"),
    }
}

/// The size to encode at for a height (the width follows the canvas, both even); `None` when
/// that is the canvas's own size or larger.
pub fn scaled(canvas: &Canvas, height: Option<u32>) -> Option<(u32, u32)> {
    let h = height?;
    // "1080p" means the short side: a vertical canvas scales its width to it.
    let (w, ch) = (canvas.width as f32, canvas.height as f32);
    let (sw, sh) =
        if canvas.height > canvas.width { (h as f32, h as f32 * ch / w) } else { (h as f32 * w / ch, h as f32) };
    let (sw, sh) = ((sw.round() as u32) & !1, (sh.round() as u32) & !1);
    (sw < canvas.width || sh < canvas.height).then_some((sw, sh))
}

/// The divisor to encode with: the one asked for if the canvas's rate allows it (nothing under
/// 10 frames a second), else the nearest that does.
pub fn fps_divisor(canvas: &Canvas, asked: u32) -> u32 {
    fps_divisor_label(asked, canvas.fps())
}

/// The divisor a destination's frame-rate chips show as chosen for a canvas at `fps` (the one
/// that's really used).
pub fn fps_divisor_label(asked: u32, fps: f64) -> u32 {
    Encoding::frame_rates(fps).iter().map(|(d, _)| *d).filter(|d| *d <= asked.max(1)).max().unwrap_or(1)
}

/// What Automatic means for a destination: the shared bitrate, capped at what its platform
/// takes; H.264, 2-second keyframes, the canvas's size.
pub fn automatic(workspace: &Workspace, d: &Destination) -> Encoding {
    let platform = d.platform.as_deref().and_then(platforms::by_id).or_else(|| platforms::guess(&d.server));
    let cap = platform.and_then(|p| p.max_kbps).unwrap_or(u32::MAX);
    Encoding { bitrate_kbps: workspace.outputs.stream_bitrate_kbps.min(cap), ..Encoding::default() }
}

/// A destination's encoders.
pub fn for_stream(caps: &[String], workspace: &Workspace, d: &Destination) -> EncoderConfig {
    let e = d.encoding.unwrap_or_else(|| automatic(workspace, d));
    let canvas = d.canvas.and_then(|f| workspace.canvas(f)).unwrap_or(workspace.primary_canvas());
    EncoderConfig {
        video_encoder: encoder_id(caps, e.encoder),
        video_bitrate_kbps: e.bitrate_kbps,
        audio_encoder: audio_id(caps),
        audio_bitrate_kbps: e.audio_kbps,
        constant_bitrate: true,
        bframes: true,
        keyint_sec: e.keyframe_s.max(1),
        scaled: scaled(canvas, e.height),
        // its sound: a mix, or the master
        audio_mix: d.audio_mix.and_then(|m| libobs_mix(workspace, m)).unwrap_or(0),
        // dynamic bitrate is RTMP's: the others handle congestion themselves
        adaptive: d.options.adaptive_bitrate
            && matches!(StreamProtocol::of(&d.server), Some(StreamProtocol::Rtmp)),
        // every Nth canvas frame, within what the canvas's rate allows
        fps_divisor: fps_divisor(canvas, e.fps_divisor),
    }
}

/// Destinations whose streams would share one encoder: same canvas, same settings, none on
/// adaptive bitrate (its output changes the bitrate). One list per encoder, in the destinations'
/// order. (The Outputs list no longer shows the groups; each destination's sheet says whom it
/// shares with. Kept for its tests and as the settings-side rule.)
#[cfg_attr(not(test), allow(dead_code))]
pub fn share_groups(caps: &[String], workspace: &Workspace, destinations: &[&Destination]) -> Vec<Vec<lightair_model::Id>> {
    let canvas_of = |d: &Destination| d.canvas.unwrap_or(workspace.primary_canvas().id);
    let mut groups: Vec<(lightair_model::Id, EncoderConfig, Vec<lightair_model::Id>)> = Vec::new();
    for d in destinations {
        let cfg = lightair_engine::outputs::stream_encoders(&d.server, &for_stream(caps, workspace, d));
        let canvas = canvas_of(d);
        let slot = (!cfg.adaptive).then(|| groups.iter_mut().find(|(c, g, _)| *c == canvas && *g == cfg)).flatten();
        match slot {
            Some((_, _, ids)) => ids.push(d.id),
            None => groups.push((canvas, cfg, vec![d.id])),
        }
    }
    groups.into_iter().map(|(_, _, ids)| ids).collect()
}

/// Why `a` and `b` can't share an encoder, in words; `None` when they can.
pub fn why_apart(caps: &[String], workspace: &Workspace, a: &Destination, b: &Destination) -> Option<&'static str> {
    let canvas_of = |d: &Destination| d.canvas.unwrap_or(workspace.primary_canvas().id);
    if canvas_of(a) != canvas_of(b) {
        return Some("canvas");
    }
    let cfg = |d: &Destination| lightair_engine::outputs::stream_encoders(&d.server, &for_stream(caps, workspace, d));
    let (ca, cb) = (cfg(a), cfg(b));
    // (adaptive destinations never share, even when everything else matches)
    ca.difference(&cb).or_else(|| ca.adaptive.then_some("adaptive bitrate"))
}

/// `d` with the settings that decide an encoder taken from `source`: its canvas, quality, audio
/// mix and adaptive bitrate. (Automatic quality is written out as what it means for `source`.)
pub fn matched_to(workspace: &Workspace, d: &Destination, source: &Destination) -> Destination {
    let mut out = d.clone();
    out.canvas = source.canvas;
    out.encoding = Some(source.encoding.unwrap_or_else(|| automatic(workspace, source)));
    out.audio_mix = source.audio_mix;
    out.options.adaptive_bitrate = source.options.adaptive_bitrate;
    out
}

/// The libobs mix a mixer mix plays into: the audio engine plays the first five mixes, in mixer
/// order, into libobs mixes 1–5 (0 is Program, the master); `None` past those or when gone.
pub fn libobs_mix(workspace: &Workspace, mix: lightair_model::Id) -> Option<usize> {
    workspace.audio.mixes.iter().position(|m| m.id == mix).filter(|&k| k < 5).map(|k| k + 1)
}

/// A recording's encoders for one canvas.
pub fn for_recording(caps: &[String], settings: &RecordingSettings, canvas: &Canvas) -> EncoderConfig {
    EncoderConfig {
        video_encoder: encoder_id(caps, settings.encoder),
        video_bitrate_kbps: settings.bitrate_kbps,
        audio_encoder: audio_id(caps),
        audio_bitrate_kbps: settings.audio_kbps,
        constant_bitrate: false,
        bframes: true,
        keyint_sec: 2,
        scaled: scaled(canvas, settings.height),
        audio_mix: 0,
        adaptive: false,
        fps_divisor: 1,
    }
}

fn audio_id(caps: &[String]) -> String {
    aac_encoder(caps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac() -> Vec<String> {
        [
            "obs_x264",
            "com.apple.videotoolbox.videoencoder.ave.avc",
            "com.apple.videotoolbox.videoencoder.ave.hevc",
            "com.apple.videotoolbox.videoencoder.appleproreshw.422",
            "CoreAudio_AAC",
        ]
        .map(String::from)
        .to_vec()
    }

    #[test]
    fn choices_map_to_this_macs_encoders() {
        let caps = mac();
        assert!(encoder_id(&caps, VideoEncoder::H264).ends_with("ave.avc"));
        assert!(encoder_id(&caps, VideoEncoder::Hevc).ends_with("ave.hevc"));
        assert!(encoder_id(&caps, VideoEncoder::ProRes).contains("prores"));
        // No HEVC here: H.264 instead, and the choice reads as unavailable.
        let old = vec!["obs_x264".to_string(), "com.apple.videotoolbox.videoencoder.h264".to_string()];
        assert!(encoder_id(&old, VideoEncoder::Hevc).ends_with("h264"));
        assert!(!available(&old, VideoEncoder::Hevc));
    }

    #[test]
    fn choices_map_to_a_pcs_encoders() {
        let nvidia: Vec<String> =
            ["obs_x264", "obs_nvenc_h264_tex", "obs_nvenc_hevc_tex", "ffmpeg_aac"].map(String::from).to_vec();
        assert_eq!(encoder_id(&nvidia, VideoEncoder::H264), "obs_nvenc_h264_tex");
        assert_eq!(encoder_id(&nvidia, VideoEncoder::Hevc), "obs_nvenc_hevc_tex");
        assert!(available(&nvidia, VideoEncoder::Hevc));
        // no ProRes in libobs on Windows: H.264, and the choice reads as unavailable
        assert_eq!(encoder_id(&nvidia, VideoEncoder::ProRes), "obs_nvenc_h264_tex");
        assert!(!available(&nvidia, VideoEncoder::ProRes));
        assert_eq!(audio_id(&nvidia), "ffmpeg_aac");
        let intel: Vec<String> = ["obs_x264", "obs_qsv11_v2"].map(String::from).to_vec();
        assert_eq!(encoder_id(&intel, VideoEncoder::H264), "obs_qsv11_v2");
        assert_eq!(encoder_id(&intel, VideoEncoder::Hevc), "obs_qsv11_v2", "no HEVC: H.264");
    }

    #[test]
    fn heights_scale_by_the_short_side() {
        let landscape = Canvas::landscape();
        assert_eq!(scaled(&landscape, Some(720)), Some((1280, 720)));
        assert_eq!(scaled(&landscape, Some(1080)), None, "its own size");
        assert_eq!(scaled(&landscape, None), None);
        let vertical = Canvas::vertical();
        assert_eq!(scaled(&vertical, Some(720)), Some((720, 1280)));
    }

    #[test]
    fn automatic_caps_at_the_platform() {
        let mut workspace = Workspace::new("P");
        workspace.outputs.stream_bitrate_kbps = 9000;
        let d = |platform: &str| Destination {
            id: lightair_model::Id::new(),
            name: platform.into(),
            server: "rtmp://example.com/live".into(),
            enabled: true,
            canvas: None,
            platform: Some(platform.into()),
            encoding: None,
            audio_mix: None,
            vod_mix: None,
            chat: None,
            options: Default::default(),
        };
        assert_eq!(automatic(&workspace, &d("twitch")).bitrate_kbps, 6000, "Twitch takes 6 Mbps");
        assert_eq!(automatic(&workspace, &d("youtube")).bitrate_kbps, 9000);
        let cfg = for_stream(&mac(), &workspace, &d("youtube"));
        assert!(cfg.constant_bitrate && cfg.keyint_sec == 2 && cfg.scaled.is_none());
    }

    fn dest(name: &str, kbps: Option<u32>) -> Destination {
        Destination {
            id: lightair_model::Id::new(),
            name: name.into(),
            server: "rtmp://example.com/live".into(),
            enabled: true,
            canvas: None,
            platform: None,
            encoding: kbps.map(|k| Encoding { bitrate_kbps: k, ..Encoding::default() }),
            audio_mix: None,
            vod_mix: None,
            chat: None,
            options: lightair_model::StreamOptions { adaptive_bitrate: false, ..Default::default() },
        }
    }

    #[test]
    fn a_destinations_frame_rate_is_part_of_what_it_shares() {
        let workspace = Workspace::new("P");
        let canvas = workspace.primary_canvas().clone();
        assert_eq!(canvas.fps(), 60.0);
        assert_eq!(fps_divisor(&canvas, 1), 1);
        assert_eq!(fps_divisor(&canvas, 2), 2, "60 to 30");
        assert_eq!(fps_divisor(&canvas, 9), 4, "no lower than the canvas allows (15 fps)");
        let slow = Canvas { fps_num: 24, fps_den: 1, ..Canvas::landscape() };
        assert_eq!(fps_divisor(&slow, 4), 2, "24 fps: 12 is as low as it goes (nothing under 10)");
        let (a, mut b) = (dest("A", Some(6000)), dest("B", Some(6000)));
        assert_eq!(why_apart(&mac(), &workspace, &a, &b), None);
        b.encoding = Some(Encoding { fps_divisor: 2, ..b.encoding.unwrap() });
        assert_eq!(for_stream(&mac(), &workspace, &b).fps_divisor, 2);
        assert_eq!(why_apart(&mac(), &workspace, &a, &b), Some("frame rate"));
    }

    #[test]
    fn destinations_with_the_same_settings_share_and_the_difference_is_named() {
        let workspace = Workspace::new("P");
        let (a, b, c) = (dest("A", Some(6000)), dest("B", Some(6000)), dest("C", Some(4500)));
        let groups = share_groups(&mac(), &workspace, &[&a, &b, &c]);
        assert_eq!(groups, vec![vec![a.id, b.id], vec![c.id]]);
        assert_eq!(why_apart(&mac(), &workspace, &a, &b), None);
        assert_eq!(why_apart(&mac(), &workspace, &a, &c), Some("bitrate"));
        // adaptive destinations keep an encoder each
        let (mut d, mut e) = (dest("D", Some(6000)), dest("E", Some(6000)));
        d.options.adaptive_bitrate = true;
        e.options.adaptive_bitrate = true;
        assert_eq!(share_groups(&mac(), &workspace, &[&d, &e]).len(), 2);
        assert_eq!(why_apart(&mac(), &workspace, &d, &e), Some("adaptive bitrate"));
    }

    #[test]
    fn matching_settings_moves_a_destination_onto_the_others_encoder() {
        let workspace = Workspace::new("P");
        let (a, c) = (dest("A", Some(6000)), dest("C", Some(4500)));
        let moved = matched_to(&workspace, &c, &a);
        assert_eq!(why_apart(&mac(), &workspace, &a, &moved), None);
        assert_eq!(share_groups(&mac(), &workspace, &[&a, &moved]).len(), 1);
        // an Automatic source is written out as what it means
        let auto = dest("Auto", None);
        let moved = matched_to(&workspace, &c, &auto);
        assert_eq!(moved.encoding, Some(automatic(&workspace, &auto)));
    }

    #[test]
    fn adaptive_bitrate_is_rtmps_and_can_be_turned_off() {
        let workspace = Workspace::new("P");
        let mut d = Destination {
            id: lightair_model::Id::new(),
            name: "T".into(),
            server: "rtmp://example.com/live".into(),
            enabled: true,
            canvas: None,
            platform: None,
            encoding: None,
            audio_mix: None,
            vod_mix: None,
            chat: None,
            options: Default::default(),
        };
        assert!(for_stream(&mac(), &workspace, &d).adaptive, "on by default for RTMP");
        d.server = "srt://example.com:9000".into();
        assert!(!for_stream(&mac(), &workspace, &d).adaptive, "SRT manages congestion itself");
        d.server = "rtmps://example.com/live".into();
        d.options.adaptive_bitrate = false;
        assert!(!for_stream(&mac(), &workspace, &d).adaptive);
    }
}
