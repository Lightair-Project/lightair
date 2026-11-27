//! `lightair://` links (catalog formats §7): opened from the website or another app, queued here
//! by the system's open-URL event and handled by the window (`StudioWindow::handle_links`).
//! A link never installs anything by itself: it opens the install screen or the add-catalog
//! question.

use std::sync::Mutex;

/// What a link asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// `lightair://install?id=…[&version=…][&catalog=…]`.
    Install { id: String, version: Option<String>, catalog: Option<String> },
    /// `lightair://catalog?url=…`.
    Catalog { url: String },
}

static PENDING: Mutex<Vec<Link>> = Mutex::new(Vec::new());

/// Queues the links the system opened (anything that isn't one of ours is ignored).
pub fn push(urls: Vec<String>) {
    let links: Vec<Link> = urls.iter().filter_map(|u| parse(u)).collect();
    if !links.is_empty() {
        PENDING.lock().unwrap_or_else(|e| e.into_inner()).extend(links);
    }
}

/// Queues a link made inside Lightair (`catalogs::request_install`).
pub fn queue(link: Link) {
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).push(link);
}

/// Takes the queued links.
pub fn take() -> Vec<Link> {
    std::mem::take(&mut *PENDING.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Whether links are waiting (checked each frame; cheap).
pub fn waiting() -> bool {
    !PENDING.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
}

pub fn parse(url: &str) -> Option<Link> {
    let rest = url.trim().strip_prefix("lightair://")?;
    let (what, query) = rest.split_once('?').unwrap_or((rest, ""));
    let params: Vec<(String, String)> = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), unescape(v)))
        .collect();
    let get = |k: &str| params.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()).filter(|v| !v.is_empty());
    match what.trim_end_matches('/') {
        "install" => Some(Link::Install { id: get("id")?, version: get("version"), catalog: get("catalog") }),
        "catalog" => Some(Link::Catalog { url: get("url")? }),
        _ => None,
    }
}

/// Percent-decoding (`%2F`, `+` as a space).
fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
