//! Lightair's pictures on Windows, on the GPU: a buffer drawn into straight by the Copier
//! (`Copier::draw`, as decoders convert into them), pictures scaled down and read back
//! (`buffers_small::Shrinker`, for the trackers and Face blur's detector), and Windows' face
//! detector finding nothing where there's no face. Each test skips when this computer has no
//! Direct3D 11 device for Lightair's buffers.
#![cfg(windows)]

use lightair_host::buffers::{self, Buffer, Copier};
use lightair_host::buffers_small::Shrinker;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11RenderTargetView, ID3D11Texture2D, ID3D11VideoDevice};
use windows::core::Interface;

/// The buffers' device, or `None` (said) when there's none here.
fn gpu(test: &str) -> Option<ID3D11Device> {
    let device = buffers::device();
    if device.is_none() {
        eprintln!("{test}: no Direct3D 11 device for Lightair's buffers here: skipped");
    }
    device
}

/// Whether the device has a video processor (what the Shrinker scales with).
fn scales(device: &ID3D11Device, test: &str) -> bool {
    let ok = device.cast::<ID3D11VideoDevice>().is_ok();
    if !ok {
        eprintln!("{test}: the device has no video processor: skipped");
    }
    ok
}

/// Fills `texture` with one colour (RGBA, 0–1), as a draw callback does.
fn clear(device: &ID3D11Device, texture: &ID3D11Texture2D, rgba: [f32; 4]) -> bool {
    let mut view: Option<ID3D11RenderTargetView> = None;
    // SAFETY: a render target view of a BGRA render target on its own device; the device's lock
    // is held by `Copier::draw` while this runs.
    unsafe {
        if device.CreateRenderTargetView(texture, None, Some(&mut view)).is_err() {
            return false;
        }
        let Some(view) = view else { return false };
        let Ok(context) = device.GetImmediateContext() else { return false };
        context.ClearRenderTargetView(&view, &rgba);
    }
    true
}

/// `buffer` read back at its own size (BGRA rows, packed).
fn read(buffer: &Buffer) -> Vec<u8> {
    Shrinker::new().read(buffer, buffer.size()).expect("a buffer reads back at its own size")
}

/// Every pixel of `bgra` within `tolerance` of `want` (BGRA).
fn all_near(bgra: &[u8], want: [u8; 4], tolerance: u8) -> bool {
    bgra.as_chunks::<4>().0.iter().all(|px| px.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= tolerance))
}

/// The pixel at (x, y) of a `width`-wide BGRA picture.
fn pixel(bgra: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let i = (y * width + x) * 4;
    [bgra[i], bgra[i + 1], bgra[i + 2], bgra[i + 3]]
}

fn near(a: [u8; 4], b: [u8; 4], tolerance: u8) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= tolerance)
}

/// What's drawn into a buffer the Copier hands out is in it when it's handed out (finished on
/// the GPU), at the size asked for.
#[test]
fn a_drawn_buffer_holds_what_was_drawn() {
    if gpu("a_drawn_buffer_holds_what_was_drawn").is_none() {
        return;
    }
    let mut copier = Copier::new();
    let red = copier.draw((64, 32), |device, texture| clear(device, texture, [1.0, 0.0, 0.0, 1.0])).expect("drawn");
    assert_eq!(red.size(), (64, 32));
    let bytes = read(&red);
    assert_eq!(bytes.len(), 64 * 32 * 4);
    assert!(all_near(&bytes, [0, 0, 255, 255], 0), "red throughout (BGRA): {:?}", &bytes[..8]);
}

/// A draw that fails hands out nothing.
#[test]
fn a_failed_draw_hands_out_nothing() {
    if gpu("a_failed_draw_hands_out_nothing").is_none() {
        return;
    }
    let mut copier = Copier::new();
    assert!(copier.draw((32, 32), |_, _| false).is_none());
    // and the copier still draws afterwards
    assert!(copier.draw((32, 32), |device, texture| clear(device, texture, [0.0, 0.0, 1.0, 1.0])).is_some());
}

/// A buffer still held isn't drawn into again: the next draw gets another, and the first keeps
/// its picture. Once every buffer is held, nothing is handed out; let one go and it is.
#[test]
fn a_held_buffer_isnt_drawn_over() {
    if gpu("a_held_buffer_isnt_drawn_over").is_none() {
        return;
    }
    let mut copier = Copier::new();
    let first = copier.draw((16, 16), |device, texture| clear(device, texture, [1.0, 0.0, 0.0, 1.0])).expect("drawn");
    let second = copier.draw((16, 16), |device, texture| clear(device, texture, [0.0, 1.0, 0.0, 1.0])).expect("drawn");
    assert_ne!(first.texture().as_raw(), second.texture().as_raw(), "another buffer");
    assert!(all_near(&read(&first), [0, 0, 255, 255], 0), "the first is still red");
    assert!(all_near(&read(&second), [0, 255, 0, 255], 0), "the second is green");

    let mut held = vec![first, second];
    while let Some(b) = copier.draw((16, 16), |_, _| true) {
        held.push(b);
        assert!(held.len() <= 64, "the copier's buffers are bounded");
    }
    held.pop();
    assert!(copier.draw((16, 16), |_, _| true).is_some(), "a buffer let go is drawn again");
}

/// A picture scaled down on the GPU reads back at the size asked for, each part where it was:
/// the left half red, the right half blue.
#[test]
fn a_picture_scaled_down_reads_back_in_place() {
    let Some(device) = gpu("a_picture_scaled_down_reads_back_in_place") else { return };
    if !scales(&device, "a_picture_scaled_down_reads_back_in_place") {
        return;
    }
    let (w, h) = (64usize, 32usize);
    let mut rgba = Vec::with_capacity(w * h * 4);
    for _y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(if x < w / 2 { &[255, 0, 0, 255] } else { &[0, 0, 255, 255] });
        }
    }
    let picture = buffers::from_rgba(&rgba, w as u32, h as u32).expect("a buffer");
    let mut shrinker = Shrinker::new();
    let small = shrinker.read(&picture, (16, 8)).expect("scaled and read back");
    assert_eq!(small.len(), 16 * 8 * 4);
    for y in 0..8 {
        // away from the edge between them, where scaling blends
        assert!(near(pixel(&small, 16, 2, y), [0, 0, 255, 255], 8), "left red at row {y}: {:?}", pixel(&small, 16, 2, y));
        assert!(near(pixel(&small, 16, 13, y), [255, 0, 0, 255], 8), "right blue at row {y}: {:?}", pixel(&small, 16, 13, y));
    }

    // the same picture again, then another size: the textures made again only when needed
    assert_eq!(shrinker.read(&picture, (16, 8)).map(|b| b.len()), Some(16 * 8 * 4));
    let other = shrinker.read(&picture, (32, 16)).expect("another size");
    assert_eq!(other.len(), 32 * 16 * 4);
    assert!(near(pixel(&other, 32, 4, 8), [0, 0, 255, 255], 8));
}

/// A flat grey stays that grey when scaled (RGB in and out: nothing converted).
#[test]
fn scaling_keeps_the_colours() {
    let Some(device) = gpu("scaling_keeps_the_colours") else { return };
    if !scales(&device, "scaling_keeps_the_colours") {
        return;
    }
    let rgba: Vec<u8> = [96u8, 128, 160, 255].repeat(80 * 60);
    let picture = buffers::from_rgba(&rgba, 80, 60).expect("a buffer");
    let small = Shrinker::new().read(&picture, (40, 30)).expect("scaled");
    assert!(all_near(&small, [160, 128, 96, 255], 3), "{:?}", &small[..4]);
}

/// Asked for more than the picture has, it reads back at the picture's own size (never scaled
/// up), and as it is.
#[test]
fn a_picture_is_never_scaled_up() {
    if gpu("a_picture_is_never_scaled_up").is_none() {
        return;
    }
    let rgba: Vec<u8> = [10u8, 20, 30, 255].repeat(24 * 12);
    let picture = buffers::from_rgba(&rgba, 24, 12).expect("a buffer");
    let back = Shrinker::new().read(&picture, (1000, 1000)).expect("read back");
    assert_eq!(back.len(), 24 * 12 * 4);
    assert!(all_near(&back, [30, 20, 10, 255], 0));
}

/// Windows' face detector finds no face in a blank picture (and nothing at all in one too small
/// or too short to look at). Passes too where Windows has no face detector.
#[test]
fn no_faces_in_a_blank_picture() {
    for fill in [0u8, 255, 128] {
        let (w, h) = (320, 180);
        let mut blank = vec![fill; w * h * 4];
        assert!(lightair_host::vision::faces(&mut blank, w, h, w * 4).is_empty(), "a blank picture of {fill}");
    }
    // rows with padding
    let mut padded = vec![0u8; 256 * 4 * 64];
    assert!(lightair_host::vision::faces(&mut padded, 200, 64, 256 * 4).is_empty());
    let mut tiny = vec![0u8; 8 * 8 * 4];
    assert!(lightair_host::vision::faces(&mut tiny, 8, 8, 32).is_empty(), "too small to look at");
    let mut short = vec![0u8; 100];
    assert!(lightair_host::vision::faces(&mut short, 64, 64, 256).is_empty(), "fewer bytes than the picture");
}
