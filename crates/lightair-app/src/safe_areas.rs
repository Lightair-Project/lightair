//! Safe-area overlays (docs/07-scenes-objects-canvas.md, "Formats"): where each platform draws
//! its own UI over the stream, so text and faces stay clear. Zones are fractions of the canvas
//! and approximate: platforms move their buttons now and then.

/// A zone: fractions of the canvas (x, y, width, height), what covers it, and whether it's a
/// guide line (outline) rather than covered space (shaded).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Zone {
    pub rect: (f32, f32, f32, f32),
    pub label: &'static str,
    pub outline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Broadcast,
    Twitch,
    TikTok,
    Shorts,
    Reels,
}

const fn zone(x: f32, y: f32, w: f32, h: f32, label: &'static str) -> Zone {
    Zone { rect: (x, y, w, h), label, outline: false }
}

impl Profile {
    pub fn label(self) -> &'static str {
        match self {
            Self::Broadcast => "Title safe",
            Self::Twitch => "Twitch",
            Self::TikTok => "TikTok",
            Self::Shorts => "Shorts",
            Self::Reels => "Reels",
        }
    }

    /// Profiles that make sense for a canvas of this shape, in the order the chip cycles.
    pub fn for_shape(width: u32, height: u32) -> &'static [Profile] {
        if height > width { &[Self::TikTok, Self::Shorts, Self::Reels] } else { &[Self::Broadcast, Self::Twitch] }
    }

    pub fn zones(self) -> &'static [Zone] {
        match self {
            Self::Broadcast => BROADCAST,
            Self::Twitch => TWITCH,
            Self::TikTok => TIKTOK,
            Self::Shorts => SHORTS,
            Self::Reels => REELS,
        }
    }
}

const BROADCAST: &[Zone] = &[
    Zone { rect: (0.035, 0.035, 0.93, 0.93), label: "Action safe", outline: true },
    Zone { rect: (0.05, 0.05, 0.90, 0.90), label: "Title safe", outline: true },
];
const TWITCH: &[Zone] = &[zone(0.0, 0.89, 1.0, 0.11, "Player controls")];
const TIKTOK: &[Zone] = &[
    zone(0.0, 0.0, 1.0, 0.08, "Tabs"),
    zone(0.86, 0.38, 0.14, 0.45, "Buttons"),
    zone(0.0, 0.80, 0.86, 0.20, "Caption"),
];
const SHORTS: &[Zone] = &[
    zone(0.0, 0.0, 1.0, 0.06, "Top bar"),
    zone(0.85, 0.45, 0.15, 0.42, "Buttons"),
    zone(0.0, 0.82, 0.85, 0.18, "Title"),
];
const REELS: &[Zone] = &[
    zone(0.0, 0.0, 1.0, 0.07, "Top bar"),
    zone(0.86, 0.50, 0.14, 0.38, "Buttons"),
    zone(0.0, 0.80, 0.86, 0.20, "Caption"),
];

/// The chip's next state: off → each profile for the shape → off.
pub fn next(current: Option<Profile>, width: u32, height: u32) -> Option<Profile> {
    let list = Profile::for_shape(width, height);
    match current.and_then(|p| list.iter().position(|q| *q == p)) {
        None => list.first().copied(),
        Some(i) => list.get(i + 1).copied(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Profile, next};

    #[test]
    fn zones_are_inside_the_canvas() {
        for p in [Profile::Broadcast, Profile::Twitch, Profile::TikTok, Profile::Shorts, Profile::Reels] {
            for z in p.zones() {
                let (x, y, w, h) = z.rect;
                assert!(x >= 0.0 && y >= 0.0 && x + w <= 1.0001 && y + h <= 1.0001, "{p:?} {}", z.label);
            }
        }
    }

    #[test]
    fn chip_cycles_through_the_shape_and_off() {
        assert_eq!(next(None, 1080, 1920), Some(Profile::TikTok));
        assert_eq!(next(Some(Profile::Reels), 1080, 1920), None);
        assert_eq!(next(Some(Profile::Broadcast), 1920, 1080), Some(Profile::Twitch));
        // A profile for the other shape restarts the cycle.
        assert_eq!(next(Some(Profile::TikTok), 1920, 1080), Some(Profile::Broadcast));
    }
}
