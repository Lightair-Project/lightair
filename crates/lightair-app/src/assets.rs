//! Icons: Lightair's own (`assets/icons/lightair-*.svg`, drawn in the Lucide style), platform
//! logos (`assets/icons/platforms`, from Simple Icons), then everything gpui-kit bundles.

use std::borrow::Cow;

use gpui_kit::*;

const OWN: &[(&str, &[u8])] = &[
    ("icons/lightair-cue.svg", include_bytes!("../assets/icons/lightair-cue.svg")),
    ("icons/lightair-safe-areas.svg", include_bytes!("../assets/icons/lightair-safe-areas.svg")),
    ("icons/platforms/youtube.svg", include_bytes!("../assets/icons/platforms/youtube.svg")),
    ("icons/platforms/youtubeshorts.svg", include_bytes!("../assets/icons/platforms/youtubeshorts.svg")),
    ("icons/platforms/twitch.svg", include_bytes!("../assets/icons/platforms/twitch.svg")),
    ("icons/platforms/facebook.svg", include_bytes!("../assets/icons/platforms/facebook.svg")),
    ("icons/platforms/kick.svg", include_bytes!("../assets/icons/platforms/kick.svg")),
    ("icons/platforms/tiktok.svg", include_bytes!("../assets/icons/platforms/tiktok.svg")),
    ("icons/platforms/instagram.svg", include_bytes!("../assets/icons/platforms/instagram.svg")),
    ("icons/platforms/x.svg", include_bytes!("../assets/icons/platforms/x.svg")),
    ("icons/platforms/linkedin.svg", include_bytes!("../assets/icons/platforms/linkedin.svg")),
];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match OWN.iter().find(|(p, _)| *p == path) {
            Some((_, data)) => Ok(Some(Cow::Borrowed(data))),
            None => gpui_kit::assets::AllAssets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut out = gpui_kit::assets::AllAssets.list(path)?;
        out.extend(OWN.iter().filter(|(p, _)| p.starts_with(path)).map(|(p, _)| SharedString::from(*p)));
        Ok(out)
    }
}
