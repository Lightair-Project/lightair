//! PTZ cameras over VISCA (docs/13, "Built-in replacements"): moving, zooming, focusing and
//! presets for the PTZ panel. Sony's VISCA over IP (UDP 52381, a header before each command)
//! or VISCA over TCP (port 5678 on many PTZOptics-style cameras). Commands go from one worker
//! thread, never the window's.

use std::collections::HashMap;
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

use lightair_model::PtzCamera;

struct Job {
    host: String,
    port: u16,
    protocol: String,
    payload: Vec<u8>,
}

fn worker() -> &'static Sender<Job> {
    static TX: OnceLock<Sender<Job>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        std::thread::Builder::new()
            .name("lightair-ptz".into())
            .spawn(move || {
                let udp = UdpSocket::bind("0.0.0.0:0").ok();
                let mut tcp: HashMap<String, TcpStream> = HashMap::new();
                let mut seq: u32 = 1;
                while let Ok(job) = rx.recv() {
                    let Some(addr) = (job.host.as_str(), job.port).to_socket_addrs().ok().and_then(|mut a| a.next()) else {
                        tracing::warn!(host = job.host, "PTZ: no such address");
                        continue;
                    };
                    if job.protocol == "visca-tcp" {
                        let key = addr.to_string();
                        if !tcp.contains_key(&key) {
                            match TcpStream::connect_timeout(&addr, Duration::from_millis(800)) {
                                Ok(s) => {
                                    let _ = s.set_write_timeout(Some(Duration::from_millis(500)));
                                    tcp.insert(key.clone(), s);
                                }
                                Err(e) => {
                                    tracing::warn!(%e, %addr, "PTZ: can't reach the camera");
                                    continue;
                                }
                            }
                        }
                        if let Some(s) = tcp.get_mut(&key)
                            && s.write_all(&job.payload).is_err()
                        {
                            tcp.remove(&key);
                        }
                    } else if let Some(u) = &udp {
                        // VISCA over IP: payload type 0x0100 (a command), its length, a sequence number
                        let mut p = vec![0x01, 0x00];
                        p.extend_from_slice(&(job.payload.len() as u16).to_be_bytes());
                        p.extend_from_slice(&seq.to_be_bytes());
                        p.extend_from_slice(&job.payload);
                        seq = seq.wrapping_add(1);
                        let _ = u.send_to(&p, addr);
                    }
                }
            })
            .ok();
        tx
    })
}

fn send(cam: &PtzCamera, payload: Vec<u8>) {
    let _ = worker().send(Job { host: cam.host.clone(), port: cam.port, protocol: cam.protocol.clone(), payload });
}

/// Pans and tilts (−1, 0 or 1 each; both 0 stops) at `speed` (0..1).
pub fn drive(cam: &PtzCamera, pan: i8, tilt: i8, speed: f32) {
    let vv = (1.0 + speed.clamp(0.0, 1.0) * 23.0).round() as u8;
    let ww = (1.0 + speed.clamp(0.0, 1.0) * 19.0).round() as u8;
    let x = match pan {
        -1 => 0x01,
        1 => 0x02,
        _ => 0x03,
    };
    let y = match tilt {
        -1 => 0x01,
        1 => 0x02,
        _ => 0x03,
    };
    send(cam, vec![0x81, 0x01, 0x06, 0x01, vv, ww, x, y, 0xFF]);
}

/// Zooms in (1), out (−1) or stops (0), at `speed` (0..1).
pub fn zoom(cam: &PtzCamera, dir: i8, speed: f32) {
    let p = (speed.clamp(0.0, 1.0) * 7.0).round() as u8;
    let z = match dir {
        1 => 0x20 | p,
        -1 => 0x30 | p,
        _ => 0x00,
    };
    send(cam, vec![0x81, 0x01, 0x04, 0x07, z, 0xFF]);
}

pub fn home(cam: &PtzCamera) {
    send(cam, vec![0x81, 0x01, 0x06, 0x04, 0xFF]);
}

pub fn autofocus(cam: &PtzCamera) {
    send(cam, vec![0x81, 0x01, 0x04, 0x38, 0x02, 0xFF]);
}

pub fn recall(cam: &PtzCamera, preset: u8) {
    send(cam, vec![0x81, 0x01, 0x04, 0x3F, 0x02, preset.min(127), 0xFF]);
}

pub fn store(cam: &PtzCamera, preset: u8) {
    send(cam, vec![0x81, 0x01, 0x04, 0x3F, 0x01, preset.min(127), 0xFF]);
}
