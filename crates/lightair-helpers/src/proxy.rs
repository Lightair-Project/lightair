//! The local proxy: every helper's only way out (plugins.md §4.11). Each helper gets a lane of
//! its own, a port on 127.0.0.1 that only its sandbox may reach, so the proxy knows who's asking.
//! HTTPS and any other TCP go through `CONNECT`; plain HTTP is forwarded. Every attempt, allowed
//! or refused, becomes an [`Event`] for the activity panel.
//!
//! Names are resolved here, and the address checked is the one connected to, so a public name
//! that resolves to a home-network or loopback address needs those permissions too.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use crossbeam_channel::Sender;

use crate::grants::Grants;

/// One connection attempt by a helper.
#[derive(Clone, Debug)]
pub struct Event {
    /// The helper's package id.
    pub helper: String,
    pub host: String,
    pub port: u16,
    pub allowed: bool,
    /// Why, in a few words ("declared host", "not declared", "home network not granted"…).
    pub reason: &'static str,
    pub at: SystemTime,
}

/// What a lane lets through.
#[derive(Clone, Debug, Default)]
pub struct Rules {
    pub any_host: bool,
    pub hosts: Vec<String>,
    pub localhost_ports: Vec<u16>,
    pub lan: bool,
}

impl From<&Grants> for Rules {
    fn from(g: &Grants) -> Rules {
        Rules { any_host: g.any_host, hosts: g.hosts.clone(), localhost_ports: g.localhost_ports.clone(), lan: g.lan }
    }
}

/// A helper's lane. Dropping it closes the port.
pub struct Lane {
    port: u16,
    stop: Arc<AtomicBool>,
}

impl Lane {
    /// Opens a lane for `helper`.
    pub fn open(helper: &str, rules: Rules, events: Sender<Event>) -> io::Result<Lane> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let (stop2, helper, rules) = (stop.clone(), helper.to_string(), Arc::new(rules));
        std::thread::Builder::new().name(format!("helper proxy {helper}")).spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((conn, _)) => {
                        let (helper, rules, events) = (helper.clone(), rules.clone(), events.clone());
                        let _ = std::thread::Builder::new()
                            .name("helper proxy connection".into())
                            .spawn(move || serve(conn, &helper, &rules, &events));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(30)),
                    Err(e) => {
                        tracing::warn!("helper proxy for {helper}: {e}");
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        })?;
        Ok(Lane { port, stop })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The proxy address a helper is given (`HTTPS_PROXY` and friends).
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for Lane {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn serve(mut conn: TcpStream, helper: &str, rules: &Rules, events: &Sender<Event>) {
    let _ = conn.set_nonblocking(false);
    let _ = conn.set_read_timeout(Some(Duration::from_secs(20)));
    let Some((head, rest)) = read_head(&mut conn) else { return };
    let text = String::from_utf8_lossy(&head).into_owned();
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split(' ');
    let (method, target, version) =
        (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or("HTTP/1.1"));

    let connect = method.eq_ignore_ascii_case("CONNECT");
    let (host, port, path) = if connect {
        match split_host_port(target, 443) {
            Some((h, p)) => (h, p, String::new()),
            None => return refuse(&mut conn, 400),
        }
    } else if let Some(after) = target.strip_prefix("http://") {
        let (authority, path) = after.split_once('/').map_or((after, "/".to_string()), |(a, p)| (a, format!("/{p}")));
        match split_host_port(authority, 80) {
            Some((h, p)) => (h, p, path),
            None => return refuse(&mut conn, 400),
        }
    } else {
        return refuse(&mut conn, 400);
    };

    let (addr, allowed, reason) = decide(&host, port, rules);
    let _ = events.send(Event {
        helper: helper.to_string(),
        host: host.clone(),
        port,
        allowed,
        reason,
        at: SystemTime::now(),
    });
    if !allowed {
        tracing::info!("helper {helper}: refused {host}:{port} ({reason})");
        return refuse(&mut conn, 403);
    }
    tracing::debug!("helper {helper}: {host}:{port} ({reason})");
    let Some(addr) = addr else { return refuse(&mut conn, 502) };
    let Ok(mut up) = TcpStream::connect_timeout(&addr, Duration::from_secs(10)) else { return refuse(&mut conn, 502) };
    let _ = conn.set_read_timeout(None);

    if connect {
        if conn.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").is_err() {
            return;
        }
        if !rest.is_empty() && up.write_all(&rest).is_err() {
            return;
        }
    } else {
        // Forward the request in origin form, without proxy headers.
        let mut out = format!("{method} {path} {version}\r\n");
        for line in lines {
            if line.is_empty() {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("proxy-") {
                continue;
            }
            out.push_str(line);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        if up.write_all(out.as_bytes()).is_err() || (!rest.is_empty() && up.write_all(&rest).is_err()) {
            return;
        }
    }
    pipe(conn, up);
}

/// Whether `host:port` may be reached, the address to use, and why.
fn decide(host: &str, port: u16, rules: &Rules) -> (Option<SocketAddr>, bool, &'static str) {
    let host_l = host.trim_matches(['[', ']']).to_ascii_lowercase();
    let addrs: Vec<SocketAddr> = match (host_l.as_str(), port).to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(_) => return (None, false, "name not found"),
    };
    let Some(addr) = addrs.first().copied() else { return (None, false, "name not found") };
    let ip = addr.ip();
    if ip.is_loopback() {
        return if rules.localhost_ports.contains(&port) {
            (Some(addr), true, "declared local port")
        } else {
            (None, false, "local port not declared")
        };
    }
    if is_private(ip) {
        return if rules.lan { (Some(addr), true, "home network") } else { (None, false, "home network not granted") };
    }
    if rules.any_host || host_matches(&host_l, &rules.hosts) {
        (Some(addr), true, "declared host")
    } else {
        (None, false, "host not declared")
    }
}

fn host_matches(host: &str, rules: &[String]) -> bool {
    rules.iter().any(|r| match r.strip_prefix("*.") {
        Some(base) => host == base || host.ends_with(&format!(".{base}")),
        None => host == r,
    })
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64
        }
        IpAddr::V6(v6) => {
            let s = v6.segments()[0];
            (s & 0xfe00) == 0xfc00 || (s & 0xffc0) == 0xfe80 || v6.is_unspecified()
        }
    }
}

fn split_host_port(s: &str, default: u16) -> Option<(String, u16)> {
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = s.strip_prefix('[') {
        let (h, after) = rest.split_once(']')?;
        let port = after.strip_prefix(':').map_or(Some(default), |p| p.parse().ok())?;
        return Some((h.to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Some((h.to_string(), p.parse().ok()?)),
        _ => Some((s.to_string(), default)),
    }
}

/// Reads up to the end of the request head (16 KB at most); returns it and any bytes after it.
fn read_head(conn: &mut TcpStream) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        let n = conn.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(i + 4);
            return Some((buf, rest));
        }
        if buf.len() > 16 * 1024 {
            return None;
        }
    }
}

fn refuse(conn: &mut TcpStream, code: u16) {
    let text = match code {
        400 => "Bad Request",
        403 => "Forbidden by Lightair: this plugin wasn't granted that address",
        _ => "Bad Gateway",
    };
    let _ =
        conn.write_all(format!("HTTP/1.1 {code} {text}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes());
    let _ = conn.shutdown(Shutdown::Both);
}

/// Copies both ways until either side closes.
fn pipe(a: TcpStream, b: TcpStream) {
    let (Ok(a2), Ok(b2)) = (a.try_clone(), b.try_clone()) else { return };
    let t = std::thread::spawn(move || copy(a2, b2));
    copy(b, a);
    let _ = t.join();
}

fn copy(mut from: TcpStream, mut to: TcpStream) {
    let _ = io::copy(&mut from, &mut to);
    let _ = to.shutdown(Shutdown::Write);
    let _ = from.shutdown(Shutdown::Read);
}
