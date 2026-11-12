//! Go Live pre-flight checks (docs/11-outputs-streaming-recording.md, "Go Live review"). The app
//! gathers facts; this turns them into plain-language results. Pure, so it's tested directly.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Warn,
    /// Going live would fail; the Go live button is disabled.
    Block,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub level: Level,
    pub title: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default)]
pub struct Destination {
    pub name: String,
    pub enabled: bool,
    pub has_key: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub destinations: Vec<Destination>,
    /// The video encoder that would be used, if one is available.
    pub encoder: Option<String>,
    /// Distinct encoders the enabled destinations need (identical ones share).
    pub encoders: u32,
    pub video_kbps: u32,
    pub audio_kbps: u32,
    /// Frames lagged in the last second, and the render rate.
    pub lagged_recent: u32,
    pub fps: f64,
    pub target_fps: f64,
    /// Microphone tracks: name and the loudest peak (dBFS) of each of the last seconds.
    pub mics: Vec<(String, Vec<f32>)>,
    /// Free space where recordings go, if recording too.
    pub record_too: bool,
    pub free_gb: Option<f64>,
}

fn check(level: Level, title: impl Into<String>, detail: impl Into<String>) -> Check {
    Check { level, title: title.into(), detail: detail.into() }
}

pub fn run(f: &Facts) -> Vec<Check> {
    let mut out = Vec::new();
    let enabled: Vec<_> = f.destinations.iter().filter(|d| d.enabled).collect();
    if enabled.is_empty() {
        out.push(check(Level::Block, "No destination on", "Turn on at least one destination above."));
    }
    for d in enabled.iter().filter(|d| !d.has_key) {
        out.push(check(Level::Block, format!("{} has no stream key", d.name), "Add its key in Outputs."));
    }
    match &f.encoder {
        Some(e) => out.push(check(Level::Ok, "Encoder ready", e.clone())),
        None => out.push(check(Level::Block, "No video encoder", "None of the supported encoders is available.")),
    }
    if f.target_fps > 0.0 && (f.lagged_recent > 0 || f.fps < f.target_fps * 0.95) {
        out.push(check(
            Level::Warn,
            "Rendering is falling behind",
            format!("{:.0} of {:.0} fps. Close heavy apps or lower the canvas size.", f.fps, f.target_fps),
        ));
    } else {
        out.push(check(Level::Ok, "Rendering keeps up", format!("{:.0} fps", f.fps)));
    }
    if !enabled.is_empty() {
        let kbps = f.encoders.max(1) * (f.video_kbps + f.audio_kbps);
        let shared = if (f.encoders as usize) < enabled.len() { ", identical destinations share encoders" } else { "" };
        out.push(check(
            Level::Ok,
            format!("About {:.1} Mbps upload", kbps as f64 / 1000.0),
            format!(
                "{} destination{}, {} encoder{}{shared}",
                enabled.len(),
                if enabled.len() == 1 { "" } else { "s" },
                f.encoders,
                if f.encoders == 1 { "" } else { "s" }
            ),
        ));
    }
    for (name, peaks) in &f.mics {
        let loudest = peaks.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        if peaks.is_empty() {
            continue;
        } else if loudest < -50.0 {
            out.push(check(
                Level::Warn,
                format!("{name} is silent"),
                "No sound in the last few seconds. Is it muted or unplugged?",
            ));
        } else if loudest > -1.0 {
            out.push(check(
                Level::Warn,
                format!("{name} is clipping"),
                "Peaks hit 0 dB. Turn its input gain down a little.",
            ));
        } else {
            out.push(check(Level::Ok, format!("{name} sounds right"), format!("Peaks around {loudest:.0} dB")));
        }
    }
    if f.record_too {
        match f.free_gb {
            Some(gb) if gb < 5.0 => {
                out.push(check(Level::Warn, "Low disk space", format!("{gb:.1} GB free for recordings")))
            }
            Some(gb) => out.push(check(Level::Ok, "Disk space", format!("{gb:.0} GB free for recordings"))),
            None => {}
        }
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.level));
    out
}

pub fn can_go_live(checks: &[Check]) -> bool {
    checks.iter().all(|c| c.level != Level::Block)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            destinations: vec![
                Destination { name: "Twitch".into(), enabled: true, has_key: true },
                Destination { name: "YouTube".into(), enabled: true, has_key: true },
            ],
            encoder: Some("Apple H.264".into()),
            encoders: 1,
            video_kbps: 6000,
            audio_kbps: 160,
            fps: 60.0,
            target_fps: 60.0,
            mics: vec![("Mic".into(), vec![-30.0, -18.0, -60.0])],
            ..Default::default()
        }
    }

    #[test]
    fn all_good() {
        let c = run(&facts());
        assert!(can_go_live(&c));
        assert!(c.iter().all(|c| c.level == Level::Ok), "{c:#?}");
        assert!(c.iter().any(|c| c.title == "About 6.2 Mbps upload" && c.detail.contains("share")));
    }

    #[test]
    fn blocks_and_warnings() {
        let mut f = facts();
        f.destinations[1].has_key = false;
        f.mics = vec![("Mic".into(), vec![-70.0, -80.0])];
        f.lagged_recent = 3;
        f.fps = 50.0;
        let c = run(&f);
        assert!(!can_go_live(&c));
        assert_eq!(c[0].title, "YouTube has no stream key", "blocking problems come first");
        assert!(c.iter().any(|c| c.title == "Mic is silent"));
        assert!(c.iter().any(|c| c.title == "Rendering is falling behind"));
        f.destinations.iter_mut().for_each(|d| d.enabled = false);
        assert!(run(&f).iter().any(|c| c.title == "No destination on"));
    }

    #[test]
    fn clipping_and_disk() {
        let mut f = facts();
        f.mics = vec![("Mic".into(), vec![-0.2])];
        f.record_too = true;
        f.free_gb = Some(2.0);
        let c = run(&f);
        assert!(c.iter().any(|c| c.title == "Mic is clipping"));
        assert!(c.iter().any(|c| c.title == "Low disk space"));
        assert!(can_go_live(&c), "warnings don't block");
    }
}
