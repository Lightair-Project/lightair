//! Streaming platforms for the Outputs panel's setup (docs/11-outputs-streaming-recording.md):
//! where each one takes a stream, where its stream key lives and which shape it wants.
//!
//! Ingest servers and key pages come from OBS's service list (`rtmp-services/services.json`)
//! where it has them. Platforms that hand every account its own server (Kick, TikTok, Instagram,
//! LinkedIn) ask for it to be pasted instead of guessing one.

/// A platform's preferred shape.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Shape {
    Landscape,
    Vertical,
}

pub struct Platform {
    /// Stored on the destination (`Destination::platform`).
    pub id: &'static str,
    pub name: &'static str,
    /// Badge: brand colour, and the logo (an icon name) or else a short mark.
    pub color: u32,
    pub mark: &'static str,
    pub logo: Option<&'static str>,
    /// (label, url). Empty: the account's own server is pasted in.
    pub servers: &'static [(&'static str, &'static str)],
    /// Where to find the server (when pasted) and the key.
    pub help: &'static str,
    pub key_link: Option<&'static str>,
    pub shape: Shape,
    /// Highest video bitrate the platform takes, kbps.
    pub max_kbps: Option<u32>,
}

pub const TWITCH_SERVERS: &[(&str, &str)] = &[
    ("Asia Pacific (Mumbai)", "rtmp://aps30.contribute.live-video.net/app/"),
    ("Asia Pacific (Seoul)", "rtmp://apn20.contribute.live-video.net/app/"),
    ("Asia Pacific (Singapore)", "rtmp://aps10.contribute.live-video.net/app/"),
    ("Asia Pacific (Sydney)", "rtmp://aps20.contribute.live-video.net/app/"),
    ("Asia Pacific (Tokyo)", "rtmp://apn10.contribute.live-video.net/app/"),
    ("Europe (Frankfurt)", "rtmp://euc10.contribute.live-video.net/app/"),
    ("Europe (Ireland)", "rtmp://euw10.contribute.live-video.net/app/"),
    ("Europe (Paris)", "rtmp://euw30.contribute.live-video.net/app/"),
    ("Europe (Stockholm)", "rtmp://eun10.contribute.live-video.net/app/"),
    ("South America (São Paulo)", "rtmp://sae10.contribute.live-video.net/app/"),
    ("US East (N. Virginia)", "rtmp://use10.contribute.live-video.net/app/"),
    ("US East (Ohio)", "rtmp://use20.contribute.live-video.net/app/"),
    ("US West (Oregon)", "rtmp://usw20.contribute.live-video.net/app/"),
];

pub const PLATFORMS: &[Platform] = &[
    Platform {
        id: "youtube",
        name: "YouTube",
        color: 0xFF0033,
        mark: "YT",
        logo: Some("platforms/youtube"),
        servers: &[
            ("Primary", "rtmps://a.rtmps.youtube.com:443/live2"),
            ("Backup", "rtmps://b.rtmps.youtube.com:443/live2?backup=1"),
        ],
        help: "YouTube Studio → Go live → Stream: copy the stream key.",
        key_link: Some("https://www.youtube.com/live_dashboard"),
        shape: Shape::Landscape,
        max_kbps: Some(51_000),
    },
    Platform {
        id: "youtube-shorts",
        name: "YouTube Shorts",
        color: 0xFF0033,
        mark: "YS",
        logo: Some("platforms/youtubeshorts"),
        servers: &[
            ("Primary", "rtmps://a.rtmps.youtube.com:443/live2"),
            ("Backup", "rtmps://b.rtmps.youtube.com:443/live2?backup=1"),
        ],
        help: "A second YouTube stream, vertical, shown as a Short. In YouTube Studio → Go live → Stream, make another stream key for it (one channel can stream to several keys at once).",
        key_link: Some("https://www.youtube.com/live_dashboard"),
        shape: Shape::Vertical,
        max_kbps: Some(51_000),
    },
    Platform {
        id: "twitch",
        name: "Twitch",
        color: 0x9146FF,
        mark: "Tw",
        logo: Some("platforms/twitch"),
        servers: TWITCH_SERVERS,
        help: "Creator Dashboard → Settings → Stream: copy the primary stream key.",
        key_link: Some("https://dashboard.twitch.tv/settings/stream"),
        shape: Shape::Landscape,
        max_kbps: Some(6_000),
    },
    Platform {
        id: "facebook",
        name: "Facebook",
        color: 0x1877F2,
        mark: "f",
        logo: Some("platforms/facebook"),
        servers: &[("Default", "rtmps://rtmp-api.facebook.com:443/rtmp/")],
        help: "Live Producer → Streaming software: copy the stream key.",
        key_link: Some("https://www.facebook.com/live/producer"),
        shape: Shape::Landscape,
        max_kbps: Some(9_000),
    },
    Platform {
        id: "kick",
        name: "Kick",
        color: 0x53FC18,
        mark: "K",
        logo: Some("platforms/kick"),
        servers: &[],
        help: "Kick dashboard → Settings → Stream: copy the Stream URL and Stream Key.",
        key_link: None,
        shape: Shape::Landscape,
        max_kbps: Some(8_000),
    },
    Platform {
        id: "tiktok",
        name: "TikTok",
        color: 0xFE2C55,
        mark: "Tk",
        logo: Some("platforms/tiktok"),
        servers: &[],
        help: "TikTok LIVE Producer (on accounts that have it): copy the Server URL and Stream Key.",
        key_link: None,
        shape: Shape::Vertical,
        max_kbps: Some(6_000),
    },
    Platform {
        id: "instagram",
        name: "Instagram",
        color: 0xE1306C,
        mark: "IG",
        logo: Some("platforms/instagram"),
        servers: &[],
        help: "Instagram on the web → Create → Live video: copy the Stream URL and Stream key.",
        key_link: None,
        shape: Shape::Vertical,
        max_kbps: Some(4_000),
    },
    Platform {
        id: "x",
        name: "X",
        color: 0x111111,
        mark: "X",
        logo: Some("platforms/x"),
        servers: &[
            ("US West: California", "rtmp://ca.pscp.tv:80/x"),
            ("US West: Oregon", "rtmp://or.pscp.tv:80/x"),
            ("US East: Virginia", "rtmp://va.pscp.tv:80/x"),
            ("South America: Brazil", "rtmp://br.pscp.tv:80/x"),
            ("EU West: France", "rtmp://fr.pscp.tv:80/x"),
            ("EU West: Ireland", "rtmp://ie.pscp.tv:80/x"),
            ("EU Central: Germany", "rtmp://de.pscp.tv:80/x"),
            ("Asia/Pacific: Australia", "rtmp://au.pscp.tv:80/x"),
            ("Asia/Pacific: India", "rtmp://in.pscp.tv:80/x"),
            ("Asia/Pacific: Japan", "rtmp://jp.pscp.tv:80/x"),
            ("Asia/Pacific: Korea", "rtmp://kr.pscp.tv:80/x"),
            ("Asia/Pacific: Singapore", "rtmp://sg.pscp.tv:80/x"),
        ],
        help: "Media Studio → Producer → Sources: create a source and copy its stream key.",
        key_link: Some("https://studio.twitter.com/producer/sources"),
        shape: Shape::Landscape,
        max_kbps: Some(12_000),
    },
    Platform {
        id: "linkedin",
        name: "LinkedIn",
        color: 0x0A66C2,
        mark: "in",
        logo: Some("platforms/linkedin"),
        servers: &[],
        help: "LinkedIn Live event → Stream settings: copy the Stream URL and Stream key.",
        key_link: None,
        shape: Shape::Landscape,
        max_kbps: Some(6_000),
    },
    Platform {
        id: "restream",
        name: "Restream",
        color: 0x2C5BFF,
        mark: "Re",
        logo: None,
        servers: &[("Automatic", "rtmp://live.restream.io/live")],
        help: "Restream → Streaming setup: copy the stream key (Restream sends it on to your channels).",
        key_link: Some("https://restream.io/settings/streaming-setup"),
        shape: Shape::Landscape,
        max_kbps: None,
    },
    Platform {
        id: "custom",
        name: "Custom",
        color: 0x3A3A42,
        mark: "··",
        logo: None,
        servers: &[],
        help: "Any rtmp(s)://, srt:// or rist:// address, or an https:// WHIP endpoint.",
        key_link: None,
        shape: Shape::Landscape,
        max_kbps: None,
    },
];

pub fn by_id(id: &str) -> Option<&'static Platform> {
    PLATFORMS.iter().find(|p| p.id == id)
}

/// Whether a destination takes a second audio track for its VODs: Twitch's VOD track (OBS
/// sends it to Twitch alone; other platforms ignore or reject a second track).
pub fn takes_vod_track(d: &lightair_core::Destination) -> bool {
    d.platform.as_deref().and_then(by_id).or_else(|| guess(&d.server)).is_some_and(|p| p.id == "twitch")
}

/// The server address's host ("a.rtmps.youtube.com" in "rtmps://a.rtmps.youtube.com:443/live2").
fn host(server: &str) -> String {
    let rest = server.split_once("://").map_or(server, |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    host.split(':').next().unwrap_or("").to_lowercase()
}

/// The platform a server address belongs to (for destinations added before platforms were
/// kept): by its host, one of the platform's listed servers or a domain the platform owns.
pub fn guess(server: &str) -> Option<&'static Platform> {
    let host = host(server);
    if host.is_empty() {
        return None;
    }
    if let Some(p) = PLATFORMS.iter().find(|p| p.servers.iter().any(|(_, s)| self::host(s) == host)) {
        return Some(p);
    }
    let under = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    // Kick's ingest is on the same network as Twitch's, under global-contribute.
    let id = if under("global-contribute.live-video.net") || under("kick.com") {
        "kick"
    } else if under("youtube.com") {
        "youtube"
    } else if under("live-video.net") || under("twitch.tv") {
        "twitch"
    } else if under("facebook.com") {
        "facebook"
    } else if under("pscp.tv") {
        "x"
    } else if under("restream.io") {
        "restream"
    } else if under("tiktok.com") || under("tiktokcdn.com") {
        "tiktok"
    } else if under("instagram.com") {
        "instagram"
    } else if under("linkedin.com") {
        "linkedin"
    } else {
        return None;
    };
    by_id(id)
}

/// A destination that's no listed platform's: any address.
pub fn custom() -> &'static Platform {
    by_id("custom").expect("the custom platform")
}

/// Which of a platform's servers to start with: for regional lists, the one nearest this
/// Mac's time zone; otherwise the first.
pub fn nearest_server(p: &Platform) -> usize {
    let zone = std::fs::read_link("/etc/localtime").ok().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    nearest_for_zone(p, &zone)
}

pub fn nearest_for_zone(p: &Platform, zone: &str) -> usize {
    let zone = zone.rsplit("zoneinfo/").next().unwrap_or(zone);
    let pick: &[&str] = match zone {
        z if z.starts_with("Asia/Kolkata") || z.starts_with("Asia/Calcutta") => &["Mumbai", "India"],
        z if z.starts_with("Asia/Seoul") => &["Seoul", "Korea", "Tokyo"],
        z if z.starts_with("Asia/Tokyo") => &["Tokyo", "Japan"],
        z if z.starts_with("Australia/") || z.starts_with("Pacific/Auckland") => &["Sydney", "Australia"],
        z if z.starts_with("Asia/") => &["Singapore"],
        z if z.starts_with("America/Sao_Paulo") || z.starts_with("America/Argentina") => &["São Paulo", "Brazil"],
        z if z.starts_with("America/Los_Angeles") || z.starts_with("America/Vancouver") => &["Oregon", "California"],
        z if z.starts_with("America/") => &["Virginia"],
        z if z.starts_with("Europe/London") || z.starts_with("Europe/Dublin") => &["Ireland"],
        z if z.starts_with("Europe/Paris") || z.starts_with("Europe/Madrid") => &["Paris", "France", "Frankfurt"],
        z if z.starts_with("Europe/Stockholm") || z.starts_with("Europe/Oslo") || z.starts_with("Europe/Helsinki") => {
            &["Stockholm", "Frankfurt"]
        }
        z if z.starts_with("Europe/") => &["Frankfurt", "Germany"],
        _ => &[],
    };
    pick.iter().find_map(|want| p.servers.iter().position(|(label, _)| label.contains(want))).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regional_servers_follow_the_time_zone() {
        let twitch = by_id("twitch").unwrap();
        let label = |zone: &str| twitch.servers[nearest_for_zone(twitch, zone)].0;
        assert!(label("/var/db/timezone/zoneinfo/Asia/Jakarta").contains("Singapore"));
        assert!(label("Europe/Berlin").contains("Frankfurt"));
        assert!(label("America/New_York").contains("Virginia"));
        assert!(label("America/Los_Angeles").contains("Oregon"));
        // No regional list: the first server.
        let yt = by_id("youtube").unwrap();
        assert_eq!(nearest_for_zone(yt, "Asia/Jakarta"), 0);
    }

    #[test]
    fn old_destinations_are_recognised_by_server() {
        assert_eq!(guess("rtmps://a.rtmps.youtube.com:443/live2").map(|p| p.id), Some("youtube"));
        assert_eq!(guess("rtmp://euc10.contribute.live-video.net/app/").map(|p| p.id), Some("twitch"));
        assert!(guess("rtmp://example.com/live").is_none());
        // a word in an address isn't a platform
        assert!(guess("rtmp://kickstart-media.example/live").is_none());
        assert_eq!(guess("rtmp://user@ingest.restream.io:1935/live").map(|p| p.id), Some("restream"));
    }
}
