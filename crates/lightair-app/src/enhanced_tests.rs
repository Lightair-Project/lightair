//! Dual Format at Go live, from the plan files in docs/plans (shaped as Twitch answers, pointed at
//! Stream Check; no Twitch traffic): what's asked for, and how a plan becomes the engine's tracks.

use lightair_model::{Canvas, Destination, EnhancedBroadcasting, Id, StreamOptions, Workspace};
use lightair_live::enhanced_broadcasting as eb;

use super::{request, tracks};

const PLAN: &str = include_str!("../../../docs/plans/twitch-dual-format-plan.json");
const ENCODER: &str = "com.apple.videotoolbox.videoencoder.ave.avc";

/// A workspace with its landscape canvas and a vertical one.
fn workspace() -> (Workspace, Id, Id) {
    let mut ws = Workspace::new("Show");
    let landscape = ws.primary_canvas().id;
    let vertical = Canvas::vertical();
    let v = vertical.id;
    ws.canvases.push(vertical);
    (ws, landscape, v)
}

fn twitch(enhanced: EnhancedBroadcasting) -> Destination {
    Destination {
        id: Id::new(),
        name: "Twitch".into(),
        server: "rtmp://live.twitch.tv/app".into(),
        enabled: true,
        canvas: None,
        platform: Some("twitch".into()),
        encoding: None,
        audio_mix: None,
        vod_mix: None,
        chat: None,
        options: StreamOptions { enhanced: Some(enhanced), ..Default::default() },
    }
}

/// The plan's encodes become the engine's video tracks in the plan's order, each made from the
/// canvas it names (landscape first, then the vertical one), its slower rate as a frame divisor.
#[test]
fn a_dual_format_plan_becomes_tracks_in_order_on_their_canvases() {
    let (ws, landscape, vertical) = workspace();
    let d = twitch(EnhancedBroadcasting { vertical_canvas: Some(vertical), ..Default::default() });
    let req = request(&d, &ws, &[ENCODER.to_string()]).expect("asked for");
    let plan = eb::read(PLAN).unwrap();
    assert!(plan.problems(req.canvases.len(), &req.encoders).is_empty(), "the plan runs here");

    let (multitrack, canvases) = tracks(&plan, &req);
    assert_eq!(canvases, [landscape, landscape, vertical, vertical], "each track from its canvas");
    let sizes: Vec<_> = multitrack.video.iter().map(|v| (v.width, v.height, v.fps_divisor)).collect();
    assert_eq!(
        sizes,
        [(1920, 1080, 1), (640, 360, 2), (1080, 1920, 1), (360, 640, 2)],
        "in the plan's order; 30 fps from a 60 fps canvas is every other frame"
    );
    assert!(multitrack.video.iter().all(|v| v.encoder == ENCODER));
    assert_eq!(multitrack.video[0].settings["bitrate"], 6000, "the encoder's settings as planned");
    assert_eq!(multitrack.audio.len(), 1);
    assert!(multitrack.bitrate_table.is_none(), "no table unless every track has one");
}

/// The person's caps, the VOD track and RTMPS go into what's asked; the canvases in track order.
#[test]
fn the_persons_caps_go_into_the_request() {
    let (ws, landscape, vertical) = workspace();
    let mut d = twitch(EnhancedBroadcasting {
        vertical_canvas: Some(vertical),
        max_bitrate_kbps: Some(8000),
        max_video_tracks: Some(4),
    });
    d.vod_mix = Some(Id::new());
    d.server = "rtmps://live.twitch.tv/app".into();
    let req = request(&d, &ws, &[ENCODER.to_string()]).unwrap();
    assert_eq!((req.max_bitrate_kbps, req.max_video_tracks), (Some(8000), Some(4)));
    assert!(req.vod_track && req.rtmps);
    let order: Vec<Id> = req.canvases.iter().map(|(id, _)| *id).collect();
    assert_eq!(order, [landscape, vertical]);

    // no caps chosen: the platform decides
    let open = request(&twitch(EnhancedBroadcasting::default()), &ws, &[]).unwrap();
    assert_eq!((open.max_bitrate_kbps, open.max_video_tracks, open.canvases.len()), (None, None, 1));
}

/// Only a landscape destination asks, and only a portrait canvas goes as Dual Format's.
#[test]
fn only_a_landscape_destination_asks_and_only_a_portrait_canvas_is_dual() {
    let (ws, landscape, vertical) = workspace();
    let mut on_vertical = twitch(EnhancedBroadcasting::default());
    on_vertical.canvas = Some(vertical);
    assert!(request(&on_vertical, &ws, &[]).is_none(), "a vertical destination goes live as usual");
    let wrong = twitch(EnhancedBroadcasting { vertical_canvas: Some(landscape), ..Default::default() });
    assert_eq!(request(&wrong, &ws, &[]).unwrap().canvases.len(), 1, "a landscape 'vertical' canvas isn't sent");
    let mut youtube = twitch(EnhancedBroadcasting::default());
    (youtube.platform, youtube.server) = (Some("youtube".into()), "rtmps://a.rtmps.youtube.com/live2".into());
    assert!(request(&youtube, &ws, &[]).is_none(), "Twitch's feature only");
}
