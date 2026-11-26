//! A video file with a rotation stored in it (a phone's portrait clip) is read upright: the
//! media player's frames (AVFoundation, turned by VideoToolbox) and the ffmpeg reader's both
//! come out at the turned size with the picture the right way round, as ffmpeg's own player
//! shows it. macOS only (the readers are AVFoundation's and the `ffmpeg` tool's there); skipped
//! without ffmpeg.
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;

const W: u32 = 128;
const H: u32 = 72;

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn ffmpeg(args: &[&str]) -> Vec<u8> {
    let out = Command::new("ffmpeg").args(["-hide_banner", "-v", "error", "-y"]).args(args).output().unwrap();
    assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// A folder of each reader's own (the tests run side by side).
fn dir(reader: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lightair-rotated-{reader}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A clip of four coloured quarters (so a picture turned the wrong way shows), with `degrees`
/// of display rotation stored in it as a phone does.
fn clip(dir: &Path, degrees: u32) -> PathBuf {
    let plain = dir.join("plain.mp4");
    if !plain.exists() {
        let (w2, h2) = (W / 2, H / 2);
        let pattern = format!(
            "color=c=red:s={W}x{H}:r=30,drawbox=x={w2}:y=0:w={w2}:h={h2}:c=green:t=fill,\
             drawbox=x=0:y={h2}:w={w2}:h={h2}:c=blue:t=fill,drawbox=x={w2}:y={h2}:w={w2}:h={h2}:c=yellow:t=fill"
        );
        let out = plain.to_string_lossy().into_owned();
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            &pattern,
            "-t",
            "1",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "h264_videotoolbox",
            "-b:v",
            "2M",
            &out,
        ]);
    }
    let rotated = dir.join(format!("rotated-{degrees}.mp4"));
    // ffmpeg's display rotation is counterclockwise
    let ccw = (360 - degrees % 360).to_string();
    let (src, dst) = (plain.to_string_lossy().into_owned(), rotated.to_string_lossy().into_owned());
    ffmpeg(&["-display_rotation", &ccw, "-i", &src, "-c", "copy", &dst]);
    rotated
}

/// The first frame as ffmpeg's own player shows it (turned by the file's rotation): RGBA.
fn upright_reference(path: &Path, size: (u32, u32)) -> Vec<u8> {
    let px = ffmpeg(&["-i", &path.to_string_lossy(), "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgba", "-"]);
    assert_eq!(px.len(), (size.0 * size.1 * 4) as usize, "ffmpeg's upright frame is {size:?}");
    px
}

/// The colour at the middle of each quarter of an RGBA picture.
fn quarters(rgba: &[u8], (w, h): (u32, u32)) -> [[u8; 3]; 4] {
    let at = |fx: f32, fy: f32| {
        let (x, y) = ((w as f32 * fx) as usize, (h as f32 * fy) as usize);
        let i = (y * w as usize + x) * 4;
        [rgba[i], rgba[i + 1], rgba[i + 2]]
    };
    [at(0.25, 0.25), at(0.75, 0.25), at(0.25, 0.75), at(0.75, 0.75)]
}

fn alike(a: [[u8; 3]; 4], b: [[u8; 3]; 4]) -> bool {
    a.iter().zip(&b).all(|(p, q)| p.iter().zip(q).all(|(x, y)| x.abs_diff(*y) <= 60))
}

fn check(degrees: u32, read: impl Fn(&str) -> Option<lightair_render::texture::PixelBuffer>, reader: &str) {
    let d = dir(reader);
    let path = clip(&d, degrees);
    let size = if degrees % 180 == 90 { (H, W) } else { (W, H) };
    let want = quarters(&upright_reference(&path, size), size);
    let frame = read(&path.to_string_lossy()).unwrap_or_else(|| panic!("{reader} reads the {degrees}° clip"));
    assert_eq!(frame.size(), size, "{reader}: the {degrees}° clip's frames are its upright size");
    let got = quarters(&frame.rgba().expect("the frame's pixels"), size);
    assert!(alike(got, want), "{reader}, {degrees}°: quarters {got:?}, upright {want:?}");
}

/// The media player's reader (stingers, files AVFoundation plays) turns each frame upright.
#[test]
fn avfoundation_frames_of_a_rotated_file_come_upright() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    for degrees in [90, 180, 270] {
        check(degrees, |p| lightair_host::media_player::Frames::open(p)?.next_frame().map(|(_, f)| f), "AVFoundation");
    }
}

/// The ffmpeg reader (WebM stingers and the like) lets FFmpeg turn the pictures, at the size the
/// probe reports.
#[test]
fn ffmpeg_frames_of_a_rotated_file_come_upright() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    for degrees in [90, 270] {
        check(degrees, |p| lightair_host::ffmpeg_media::Frames::open(p)?.next_frame().map(|(_, f)| f), "ffmpeg");
        let probed = lightair_render::media::probe(&dir("ffmpeg").join(format!("rotated-{degrees}.mp4"))).unwrap();
        assert_eq!((probed.width, probed.height), (H, W), "the probe reports the upright size");
    }
}
