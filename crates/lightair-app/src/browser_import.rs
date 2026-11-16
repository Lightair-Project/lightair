//! A sign-in brought over from the person's own browser (Chrome, Brave, Edge, Arc on macOS),
//! for a platform whose sign-in won't work in Lightair's window (passkeys with Touch ID): only
//! that platform's cookies, read from a copy of the browser's cookie store and decrypted with
//! its "Safe Storage" key (macOS asks once whether Lightair may), then given to Lightair's
//! browser. Nothing else is read or kept.

/// (name, folder under Application Support, Keychain service, account)
#[cfg(target_os = "macos")]
const BROWSERS: &[(&str, &str, &str, &str)] = &[
    ("Chrome", "Google/Chrome", "Chrome Safe Storage", "Chrome"),
    ("Brave", "BraveSoftware/Brave-Browser", "Brave Safe Storage", "Brave"),
    ("Edge", "Microsoft Edge", "Microsoft Edge Safe Storage", "Microsoft Edge"),
    ("Arc", "Arc/User Data", "Arc Safe Storage", "Arc"),
];

/// The sites a platform's sign-in lives on.
pub fn domains(platform: &str) -> &'static [&'static str] {
    match platform {
        "twitch" => &["twitch.tv"],
        "youtube" | "youtube-shorts" => &["google.com", "youtube.com"],
        "kick" => &["kick.com"],
        "tiktok" => &["tiktok.com"],
        _ => &[],
    }
}

#[cfg(target_os = "macos")]
fn store(folder: &str) -> Option<std::path::PathBuf> {
    let base = std::env::var_os("HOME").map(std::path::PathBuf::from)?.join("Library/Application Support").join(folder).join("Default");
    [base.join("Cookies"), base.join("Network/Cookies")].into_iter().find(|p| p.exists())
}

/// The browsers here that have a cookie store.
pub fn installed() -> Vec<&'static str> {
    #[cfg(target_os = "macos")]
    {
        BROWSERS.iter().filter(|b| store(b.1).is_some()).map(|b| b.0).collect()
    }
    #[cfg(not(target_os = "macos"))]
    Vec::new()
}

#[cfg(target_os = "macos")]
fn decrypt(key: &[u8; 16], host: &str, data: &[u8]) -> Option<String> {
    use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
    use sha2::Digest;
    let body = data.strip_prefix(b"v10")?;
    let mut buf = body.to_vec();
    let plain = cbc::Decryptor::<aes::Aes128>::new(key.into(), &[b' '; 16].into()).decrypt_padded_mut::<Pkcs7>(&mut buf).ok()?;
    // newer Chrome puts the host's SHA-256 first
    let hash = sha2::Sha256::digest(host.as_bytes());
    let plain = plain.strip_prefix(hash.as_slice()).unwrap_or(plain);
    String::from_utf8(plain.to_vec()).ok()
}

/// Reads `platform`'s cookies from `browser` (asks macOS for its key: may wait on the person):
/// the cookies, ready for Lightair's browser.
#[cfg(target_os = "macos")]
pub fn read(browser: &str, platform: &str) -> Result<Vec<lightair_web::CookieSpec>, String> {
    let (name, folder, service, account) = *BROWSERS.iter().find(|b| b.0 == browser).ok_or("unknown browser")?;
    let db = store(folder).ok_or_else(|| format!("{name} isn't installed"))?;
    let wanted = domains(platform);
    if wanted.is_empty() {
        return Err(format!("signing in to {platform} isn't supported"));
    }
    let out = std::process::Command::new("/usr/bin/security").args(["find-generic-password", "-w", "-s", service, "-a", account]).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("macOS didn't give Lightair {name}'s key (Always Allow or Allow, then try again)"));
    }
    let password = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password.as_bytes(), b"saltysalt", 1003, &mut key);
    // a copy: the browser keeps its own locked
    let copy = std::env::temp_dir().join(format!("lightair-cookies-{}.db", std::process::id()));
    std::fs::copy(&db, &copy).map_err(|e| format!("couldn't read {name}'s cookies: {e}"))?;
    let filter = wanted.iter().map(|d| format!("host_key = '{d}' OR host_key LIKE '%.{d}'")).collect::<Vec<_>>().join(" OR ");
    let sql = format!("SELECT host_key, name, hex(encrypted_value), path, expires_utc, is_secure, is_httponly, samesite, value FROM cookies WHERE {filter}");
    let out = std::process::Command::new("/usr/bin/sqlite3").args(["-readonly", "-separator", "\u{1f}"]).arg(&copy).arg(&sql).output();
    let _ = std::fs::remove_file(&copy);
    let out = out.map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut list = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\u{1f}').collect();
        let [host, cname, hex, path, expires, secure, http_only, same_site, plain] = f.as_slice() else { continue };
        let bytes: Vec<u8> = (0..hex.len() / 2).filter_map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()).collect();
        let value = if bytes.is_empty() { plain.to_string() } else { match decrypt(&key, host, &bytes) { Some(v) => v, None => continue } };
        list.push(lightair_web::CookieSpec {
            url: format!("https://{}{}", host.trim_start_matches('.'), if path.is_empty() { "/" } else { path }),
            name: cname.to_string(),
            value,
            // a host-only cookie stays one
            domain: if host.starts_with('.') { host.to_string() } else { String::new() },
            path: path.to_string(),
            secure: *secure == "1",
            http_only: *http_only == "1",
            expires: expires.parse().unwrap_or(0),
            same_site: same_site.parse::<i32>().map_or(0, |s| s + 1),
        });
    }
    if list.is_empty() {
        return Err(format!("{name} isn't signed in to {platform}"));
    }
    Ok(list)
}

#[cfg(not(target_os = "macos"))]
pub fn read(_: &str, _: &str) -> Result<Vec<lightair_web::CookieSpec>, String> {
    Err("bringing a sign-in over from a browser is macOS-only for now".into())
}

/// Reads and gives Lightair's browser `platform`'s sign-in from `browser`: how many cookies.
pub fn import(browser: &str, platform: &str) -> Result<usize, String> {
    let list = read(browser, platform)?;
    let (tx, rx) = std::sync::mpsc::channel();
    lightair_web::set_cookies(list, move |n| {
        let _ = tx.send(n);
    });
    let n = rx.recv_timeout(std::time::Duration::from_secs(10)).map_err(|_| "Lightair's browser didn't answer".to_string())?;
    if n == 0 {
        return Err("Lightair's browser took none of them".into());
    }
    Ok(n)
}

#[cfg(all(test, target_os = "macos"))]
mod checks {
    use super::{decrypt, domains};
    use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
    use sha2::Digest;

    fn seal(key: &[u8; 16], plain: &[u8]) -> Vec<u8> {
        let mut out = b"v10".to_vec();
        let mut buf = vec![0u8; plain.len() + 16];
        buf[..plain.len()].copy_from_slice(plain);
        let n = cbc::Encryptor::<aes::Aes128>::new(key.into(), &[b' '; 16].into()).encrypt_padded_mut::<Pkcs7>(&mut buf, plain.len()).unwrap().len();
        out.extend(&buf[..n]);
        out
    }

    #[std::prelude::v1::test]
    fn reads_old_and_new_chrome_values() {
        let mut key = [0u8; 16];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(b"peanuts", b"saltysalt", 1003, &mut key);
        assert_eq!(decrypt(&key, ".twitch.tv", &seal(&key, b"abc123")).as_deref(), Some("abc123"));
        let mut hashed = sha2::Sha256::digest(b".twitch.tv").to_vec();
        hashed.extend(b"abc123");
        assert_eq!(decrypt(&key, ".twitch.tv", &seal(&key, &hashed)).as_deref(), Some("abc123"));
        assert_eq!(decrypt(&[0; 16], ".twitch.tv", &seal(&key, b"abc123")), None);
        assert!(domains("youtube").contains(&"google.com"));
        assert!(domains("bilibili").is_empty());
    }
}
