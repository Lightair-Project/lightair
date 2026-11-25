//! Web pages (the browser source and the HTML widgets drawn by a page) as a picture provider.
//!
//! The page is the one libobs's web page source already runs (`lightair_obs::web`): no second
//! browser. libobs still opens and closes it as it's shown and hidden (so "close off air", its
//! size, zoom, custom CSS and transparency are the source's own) and still plays its sound. While
//! a compositor shows the page, each frame Chromium paints is copied on the GPU into a BGRA
//! IOSurface of the source's own (premultiplied, transparent where the page is), and the newest
//! finished copy is handed over here. When the compositor draws it in libobs's place (no effect
//! libobs would have to draw over it), libobs stops drawing the page itself.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use lightair_model::{Id, Source};
use lightair_obs::web;
use parking_lot::Mutex;

use crate::providers::{Frame, Provider};

/// How long a page's copies are kept once no canvas shows it (a scene being rebuilt, a
/// transition's hand-over).
const LINGER: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(crate) struct Web {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The pages each canvas shows now.
    shown: HashMap<Id, HashSet<Id>>,
    /// Every page a canvas has shown lately.
    pages: HashMap<Id, Page>,
}

struct Page {
    /// Its picture has effects (libobs may draw it through them, so it keeps drawing the page).
    effects: bool,
    /// When no canvas showed it any more (`None`: one does).
    hidden_since: Option<Instant>,
}

/// Whether `source` has an effect that changes its picture (sound effects don't).
fn has_picture_effects(source: &Source) -> bool {
    source.effects.iter().any(|e| e.enabled && !e.kind.starts_with("audio."))
}

impl Provider for Web {
    fn handles(&self, source: &Source) -> bool {
        web::feed(&source.id.to_string()).is_some()
    }

    fn want(&self, canvas: Id, shown: &[&Source]) {
        let mut state = self.state.lock();
        let state = &mut *state;
        state.shown.insert(canvas, shown.iter().map(|s| s.id).collect());
        state.shown.retain(|_, ids| !ids.is_empty());
        for source in shown {
            let page = state.pages.entry(source.id).or_insert(Page { effects: false, hidden_since: None });
            page.effects = has_picture_effects(source);
        }
        let now = Instant::now();
        let shown_now: HashSet<Id> = state.shown.values().flatten().copied().collect();
        state.pages.retain(|id, page| {
            let feed = web::feed(&id.to_string());
            if shown_now.contains(id) {
                page.hidden_since = None;
                // (cheap when already wanted; a source made again since is a new feed)
                let Some(f) = feed else { return false };
                f.want(true);
                return true;
            }
            let since = *page.hidden_since.get_or_insert(now);
            if now.duration_since(since) < LINGER {
                return true;
            }
            if let Some(f) = feed {
                f.want(false);
            }
            false
        });
    }

    fn frame(&self, source: Id) -> Option<Frame> {
        let take = self.state.lock().pages.get(&source).is_some_and(|p| !p.effects && p.hidden_since.is_none());
        let (buffer, serial) = web::feed(&source.to_string())?.frame(take)?;
        Some(Frame { buffer, serial })
    }
}
