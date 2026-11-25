//! The program served over RTSP (docs/13, "Built-in replacements"): `rtsp://<this computer>:8554/live`
//! for VLC, a hardware decoder, a video wall or another app. Encoders of its own (H.264 without
//! B-frames, AAC) feed a Lightair output whose packets go to every player that has asked:
//! RTP over the RTSP connection itself (interleaved; RFC 2326 §10.12), H.264 by RFC 6184 (single
//! NAL units and FU-A) and AAC by RFC 3640 (AAC-hbr). Lightair's own code.
//!
//! macOS, with `LIGHTAIR_ENGINE=own`: the packets come from Lightair's own encoders of the
//! primary canvas's compositor frames instead (`own_outputs::start_rtsp`, which shares an
//! encoder already running when one fits), through [`own_video`] and [`own_audio`].

use std::ffi::{c_char, c_void};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};

use base64::Engine as _;

use crate::sys;

pub const PORT: u16 = 8554;
const MTU: usize = 1400;

/// A player that has pressed play: where its RTP goes.
struct Player {
    tx: SyncSender<Vec<u8>>,
    seq: [u16; 2],
}

#[derive(Default)]
struct Shared {
    players: Vec<Player>,
    /// H.264 SPS and PPS, AAC's AudioSpecificConfig, and the audio rate and channels.
    sps: Vec<u8>,
    pps: Vec<u8>,
    asc: Vec<u8>,
    audio: (u32, u32),
}

fn shared() -> &'static Arc<Mutex<Shared>> {
    static S: OnceLock<Arc<Mutex<Shared>>> = OnceLock::new();
    S.get_or_init(|| Arc::new(Mutex::new(Shared::default())))
}

/// NAL units of an H.264 packet (Annex B start codes, or 4-byte lengths).
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    if data.len() > 4 && (data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1])) {
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 3 <= data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                starts.push(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }
        for (k, &s) in starts.iter().enumerate() {
            let mut e = starts.get(k + 1).map_or(data.len(), |n| n - 3);
            // a 4-byte start code's leading zero belongs to it
            while e > s && data[e - 1] == 0 {
                e -= 1;
            }
            if e > s {
                out.push(&data[s..e]);
            }
        }
    } else {
        let mut i = 0;
        while i + 4 <= data.len() {
            let n = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
            i += 4;
            if n == 0 || i + n > data.len() {
                break;
            }
            out.push(&data[i..i + n]);
            i += n;
        }
    }
    out
}

/// SPS and PPS from an encoder's header (Annex B, or an avcC record).
fn parameter_sets(header: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (mut sps, mut pps) = (Vec::new(), Vec::new());
    if header.first() == Some(&1) && header.len() > 7 {
        // avcC: version, profile, compat, level, length size, SPS count + SPSs, PPS count + PPSs
        let mut i = 5;
        let n_sps = header[i] & 0x1f;
        i += 1;
        for _ in 0..n_sps {
            let len = u16::from_be_bytes([header[i], *header.get(i + 1).unwrap_or(&0)]) as usize;
            i += 2;
            if sps.is_empty() {
                sps = header.get(i..i + len).unwrap_or(&[]).to_vec();
            }
            i += len;
        }
        let n_pps = *header.get(i).unwrap_or(&0);
        i += 1;
        for _ in 0..n_pps {
            let len = u16::from_be_bytes([*header.get(i).unwrap_or(&0), *header.get(i + 1).unwrap_or(&0)]) as usize;
            i += 2;
            if pps.is_empty() {
                pps = header.get(i..i + len).unwrap_or(&[]).to_vec();
            }
            i += len;
        }
    } else {
        for nal in nal_units(header) {
            match nal[0] & 0x1f {
                7 if sps.is_empty() => sps = nal.to_vec(),
                8 if pps.is_empty() => pps = nal.to_vec(),
                _ => {}
            }
        }
    }
    (sps, pps)
}

fn rtp(channel: u8, payload_type: u8, marker: bool, seq: u16, ts: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let len = 12 + payload.len();
    let mut p = Vec::with_capacity(4 + len);
    p.extend_from_slice(&[b'$', channel, (len >> 8) as u8, len as u8]);
    p.push(0x80);
    p.push(payload_type | if marker { 0x80 } else { 0 });
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&ts.to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

fn send_all(s: &mut Shared, build: impl Fn(&mut [u16; 2]) -> Vec<Vec<u8>>) {
    s.players.retain_mut(|p| {
        for pkt in build(&mut p.seq) {
            match p.tx.try_send(pkt) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
        true
    });
}

/// NAL units each after a 4-byte length (as Lightair's encoders give them).
fn length_prefixed(mut data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    while data.len() >= 4 {
        let n = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        data = &data[4..];
        if n > data.len() {
            break;
        }
        if n > 0 {
            out.push(&data[..n]);
        }
        data = &data[n..];
    }
    out
}

fn video_packet(s: &mut Shared, nals: Vec<&[u8]>, ts: u32) {
    let nals: Vec<Vec<u8>> = nals.into_iter().map(|n| n.to_vec()).collect();
    // a keyframe without its parameter sets: they go first (a player joining now can start)
    let key = nals.iter().any(|n| n[0] & 0x1f == 5);
    let has_sps = nals.iter().any(|n| n[0] & 0x1f == 7);
    let mut all = Vec::new();
    if key && !has_sps && !s.sps.is_empty() {
        all.push(s.sps.clone());
        all.push(s.pps.clone());
    }
    all.extend(nals);
    send_all(s, |seq| {
        let mut out = Vec::new();
        for (k, nal) in all.iter().enumerate() {
            let last = k + 1 == all.len();
            if nal.len() <= MTU {
                out.push(rtp(0, 96, last, seq[0], ts, 0x4c41_0001, nal));
                seq[0] = seq[0].wrapping_add(1);
            } else {
                // FU-A
                let (head, body) = (nal[0], &nal[1..]);
                let chunks: Vec<&[u8]> = body.chunks(MTU - 2).collect();
                for (i, c) in chunks.iter().enumerate() {
                    let (first, end) = (i == 0, i + 1 == chunks.len());
                    let mut pl = Vec::with_capacity(c.len() + 2);
                    pl.push((head & 0xe0) | 28);
                    pl.push((head & 0x1f) | if first { 0x80 } else { 0 } | if end { 0x40 } else { 0 });
                    pl.extend_from_slice(c);
                    out.push(rtp(0, 96, last && end, seq[0], ts, 0x4c41_0001, &pl));
                    seq[0] = seq[0].wrapping_add(1);
                }
            }
        }
        out
    });
}

fn audio_packet(s: &mut Shared, data: &[u8], ts: u32) {
    if data.is_empty() || data.len() > 8191 {
        return;
    }
    let mut pl = Vec::with_capacity(data.len() + 4);
    pl.extend_from_slice(&16u16.to_be_bytes());
    pl.extend_from_slice(&((data.len() as u16) << 3).to_be_bytes());
    pl.extend_from_slice(data);
    send_all(s, |seq| {
        let p = rtp(2, 97, true, seq[1], ts, 0x4c41_0002, &pl);
        seq[1] = seq[1].wrapping_add(1);
        vec![p]
    });
}

fn sdp(s: &Shared, host: &str) -> String {
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let profile = s.sps.get(1..4).map_or("42e01f".to_string(), |p| format!("{:02x}{:02x}{:02x}", p[0], p[1], p[2]));
    let mut out = format!(
        "v=0\r\no=- 0 0 IN IP4 {host}\r\ns=Lightair\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\na=control:*\r\n\
         m=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 packetization-mode=1;profile-level-id={profile};sprop-parameter-sets={},{}\r\na=control:trackID=0\r\n",
        b64(&s.sps),
        b64(&s.pps)
    );
    if !s.asc.is_empty() {
        let config: String = s.asc.iter().map(|b| format!("{b:02x}")).collect();
        out.push_str(&format!(
            "m=audio 0 RTP/AVP 97\r\na=rtpmap:97 MPEG4-GENERIC/{}/{}\r\na=fmtp:97 streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config={config}\r\na=control:trackID=1\r\n",
            s.audio.0, s.audio.1
        ));
    }
    out
}

/// One player's RTSP conversation; after PLAY its connection carries the RTP too.
fn serve(stream: TcpStream) {
    let Ok(writer) = stream.try_clone() else { return };
    let writer = Arc::new(Mutex::new(writer));
    let mut reader = BufReader::new(stream);
    let host = lightair_platform::net::local_ip().unwrap_or_else(|| "127.0.0.1".into());
    let session = format!("{:08x}", std::process::id() ^ (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos())));
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let request = line.trim().to_string();
        if request.is_empty() {
            continue;
        }
        let mut cseq = String::from("0");
        let mut transport = String::new();
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                match k.trim().to_ascii_lowercase().as_str() {
                    "cseq" => cseq = v.trim().into(),
                    "transport" => transport = v.trim().into(),
                    _ => {}
                }
            }
        }
        let method = request.split_whitespace().next().unwrap_or("");
        let reply = |status: &str, headers: &str, body: &str| {
            let mut r = format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\nServer: Lightair\r\n{headers}");
            if !body.is_empty() {
                r.push_str(&format!("Content-Type: application/sdp\r\nContent-Length: {}\r\n", body.len()));
            }
            r.push_str("\r\n");
            r.push_str(body);
            r
        };
        let answer = match method {
            "OPTIONS" => reply("200 OK", "Public: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN\r\n", ""),
            "DESCRIBE" => {
                let body = shared().lock().map(|s| sdp(&s, &host)).unwrap_or_default();
                let base = request.split_whitespace().nth(1).unwrap_or("");
                reply("200 OK", &format!("Content-Base: {base}/\r\n"), &body)
            }
            "SETUP" => {
                // RTP over this connection only (players then ask for it)
                if !transport.contains("TCP") {
                    reply("461 Unsupported Transport", "", "")
                } else {
                    let channels = if request.contains("trackID=1") { "2-3" } else { "0-1" };
                    reply("200 OK", &format!("Transport: RTP/AVP/TCP;unicast;interleaved={channels}\r\nSession: {session}\r\n"), "")
                }
            }
            "PLAY" => {
                KEYFRAME_WANTED.store(true, Ordering::Release);
                let (tx, rx) = sync_channel::<Vec<u8>>(512);
                if let Ok(mut s) = shared().lock() {
                    s.players.push(Player { tx, seq: [0, 0] });
                }
                let w = writer.clone();
                std::thread::spawn(move || {
                    while let Ok(p) = rx.recv() {
                        let Ok(mut w) = w.lock() else { return };
                        if w.write_all(&p).is_err() {
                            return;
                        }
                    }
                });
                reply("200 OK", &format!("Session: {session}\r\nRange: npt=0.000-\r\n"), "")
            }
            "TEARDOWN" => {
                let r = reply("200 OK", &format!("Session: {session}\r\n"), "");
                if let Ok(mut w) = writer.lock() {
                    let _ = w.write_all(r.as_bytes());
                }
                return;
            }
            _ => reply("405 Method Not Allowed", "", ""),
        };
        let Ok(mut w) = writer.lock() else { return };
        if w.write_all(answer.as_bytes()).is_err() {
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The output that gets the encoded packets

unsafe extern "C" fn out_name(_: *mut c_void) -> *const c_char {
    c"RTSP server".as_ptr()
}

unsafe extern "C" fn out_create(_: *mut sys::obs_data_t, output: *mut sys::obs_output_t) -> *mut c_void {
    Box::into_raw(Box::new(output as usize)) as *mut c_void
}

unsafe extern "C" fn out_destroy(data: *mut c_void) {
    drop(unsafe { Box::from_raw(data as *mut usize) });
}

unsafe extern "C" fn out_start(data: *mut c_void) -> bool {
    let output = unsafe { *(data as *const usize) } as *mut sys::obs_output_t;
    unsafe {
        if !sys::obs_output_can_begin_data_capture(output, 0) || !sys::obs_output_initialize_encoders(output, 0) {
            return false;
        }
        // the parameter sets and the audio's config, for the description
        let venc = sys::obs_output_get_video_encoder(output);
        let aenc = sys::obs_output_get_audio_encoder(output, 0);
        if let Ok(mut s) = shared().lock() {
            let mut p: *mut u8 = std::ptr::null_mut();
            let mut n = 0usize;
            if sys::obs_encoder_get_extra_data(venc, &mut p, &mut n) && !p.is_null() {
                (s.sps, s.pps) = parameter_sets(std::slice::from_raw_parts(p, n));
            }
            if !aenc.is_null() && sys::obs_encoder_get_extra_data(aenc, &mut p, &mut n) && !p.is_null() {
                s.asc = std::slice::from_raw_parts(p, n).to_vec();
                s.audio = (sys::obs_encoder_get_sample_rate(aenc), 2);
            }
        }
        sys::obs_output_begin_data_capture(output, 0)
    }
}

unsafe extern "C" fn out_stop(data: *mut c_void, _: u64) {
    let output = unsafe { *(data as *const usize) } as *mut sys::obs_output_t;
    unsafe { sys::obs_output_end_data_capture(output) };
}

unsafe extern "C" fn out_packet(_: *mut c_void, packet: *mut sys::encoder_packet) {
    let Some(p) = (unsafe { packet.as_ref() }) else { return };
    if p.data.is_null() || p.size == 0 {
        return;
    }
    let data = unsafe { std::slice::from_raw_parts(p.data, p.size) };
    let Ok(mut s) = shared().lock() else { return };
    if s.players.is_empty() {
        // learn parameter sets from a keyframe even with nobody watching
        if p.type_ == sys::OBS_ENCODER_VIDEO && s.sps.is_empty() {
            for nal in nal_units(data) {
                match nal[0] & 0x1f {
                    7 => s.sps = nal.to_vec(),
                    8 => s.pps = nal.to_vec(),
                    _ => {}
                }
            }
        }
        return;
    }
    let seconds = |t: i64| t as f64 * p.timebase_num as f64 / p.timebase_den.max(1) as f64;
    if p.type_ == sys::OBS_ENCODER_VIDEO {
        let ts = (seconds(p.pts) * 90_000.0) as i64 as u32;
        video_packet(&mut s, nal_units(data), ts);
    } else {
        let rate = s.audio.0.max(1) as f64;
        let ts = (seconds(p.pts) * rate) as i64 as u32;
        audio_packet(&mut s, data, ts);
    }
}

// ---------------------------------------------------------------------------------------------
// Packets from Lightair's own encoders

/// A player pressed play since this was last asked (its picture starts at a keyframe).
static KEYFRAME_WANTED: AtomicBool = AtomicBool::new(false);

/// Whether a player is waiting for a keyframe (asked once per player).
pub(crate) fn take_keyframe_request() -> bool {
    KEYFRAME_WANTED.swap(false, Ordering::AcqRel)
}

/// The H.264 parameter sets (bare SPS and PPS) of the encoder feeding the server.
pub(crate) fn own_parameter_sets(sets: &[Vec<u8>]) {
    let Ok(mut s) = shared().lock() else { return };
    for set in sets {
        match set.first().map(|b| b & 0x1f) {
            Some(7) => s.sps = set.clone(),
            Some(8) => s.pps = set.clone(),
            _ => {}
        }
    }
}

/// The AAC encoder's AudioSpecificConfig, sample rate and channels.
pub(crate) fn own_audio_config(asc: &[u8], rate: u32, channels: u32) {
    if let Ok(mut s) = shared().lock() {
        s.asc = asc.to_vec();
        s.audio = (rate, channels);
    }
}

/// One encoded picture (NAL units each after a 4-byte length), `at_ns` after the server's
/// clocks started.
pub(crate) fn own_video(data: &[u8], at_ns: i64) {
    let Ok(mut s) = shared().lock() else { return };
    if s.players.is_empty() {
        return;
    }
    let ts = (at_ns.max(0) as i128 * 90_000 / 1_000_000_000) as i64 as u32;
    video_packet(&mut s, length_prefixed(data), ts);
}

/// One raw AAC frame, `at_ns` after the server's clocks started.
pub(crate) fn own_audio(data: &[u8], at_ns: i64) {
    let Ok(mut s) = shared().lock() else { return };
    if s.players.is_empty() {
        return;
    }
    let rate = s.audio.0.max(1) as i128;
    let ts = (at_ns.max(0) as i128 * rate / 1_000_000_000) as i64 as u32;
    audio_packet(&mut s, data, ts);
}

pub fn register() {
    let info = sys::obs_output_info {
        id: c"lightair_rtsp".as_ptr(),
        flags: sys::OBS_OUTPUT_AV | sys::OBS_OUTPUT_ENCODED,
        get_name: Some(out_name),
        create: Some(out_create),
        destroy: Some(out_destroy),
        start: Some(out_start),
        stop: Some(out_stop),
        encoded_packet: Some(out_packet),
        encoded_video_codecs: c"h264".as_ptr(),
        encoded_audio_codecs: c"aac".as_ptr(),
        ..Default::default()
    };
    unsafe { sys::obs_register_output_s(&info, std::mem::size_of::<sys::obs_output_info>()) };
}

enum Running {
    /// libobs's encoders and the `lightair_rtsp` output.
    Libobs { output: *mut sys::obs_output_t, video: *mut sys::obs_encoder_t, audio: *mut sys::obs_encoder_t },
    /// Lightair's own encoders (macOS, `LIGHTAIR_ENGINE=own`).
    #[cfg(target_os = "macos")]
    Own,
}

unsafe impl Send for Running {}

fn running() -> &'static Mutex<Option<Running>> {
    static R: OnceLock<Mutex<Option<Running>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(None))
}

/// The address players open, when it's on.
pub fn address() -> Option<String> {
    running().lock().ok()?.as_ref()?;
    Some(format!("rtsp://{}:{PORT}/live", lightair_platform::net::local_ip().unwrap_or_else(|| "127.0.0.1".into())))
}

/// Serves the program over RTSP (or stops). Call on the engine thread.
pub fn set(on: bool, available_encoders: &[String]) -> Result<(), String> {
    let mut r = running().lock().map_err(|_| "busy".to_string())?;
    if let Some(old) = r.take() {
        match old {
            Running::Libobs { output, video, audio } => unsafe {
                sys::obs_output_stop(output);
                sys::obs_output_release(output);
                sys::obs_encoder_release(video);
                sys::obs_encoder_release(audio);
            },
            #[cfg(target_os = "macos")]
            Running::Own => crate::own_outputs::stop_rtsp(),
        }
        if let Ok(mut s) = shared().lock() {
            s.players.clear();
            (s.sps, s.pps, s.asc) = (Vec::new(), Vec::new(), Vec::new());
        }
    }
    if !on {
        return Ok(());
    }
    // the server, once
    static LISTENING: OnceLock<bool> = OnceLock::new();
    let ok = *LISTENING.get_or_init(|| match TcpListener::bind(("0.0.0.0", PORT)) {
        Ok(l) => {
            std::thread::Builder::new()
                .name("lightair-rtsp".into())
                .spawn(move || {
                    for s in l.incoming().flatten() {
                        let _ = s.set_nodelay(true);
                        std::thread::spawn(move || serve(s));
                    }
                })
                .is_ok()
        }
        Err(e) => {
            tracing::warn!(%e, "RTSP: port {PORT} is taken");
            false
        }
    });
    if !ok {
        return Err(format!("Port {PORT} is taken by another app"));
    }
    // Lightair's own encoders, from the primary canvas's compositor
    #[cfg(target_os = "macos")]
    if crate::own_outputs::enabled()
        && let Some(canvas) = crate::program_source::canvas_of_video(None)
    {
        crate::own_outputs::start_rtsp(canvas)?;
        *r = Some(Running::Own);
        tracing::info!(port = PORT, "serving the program over RTSP (Lightair's encoders)");
        return Ok(());
    }
    let venc_id = crate::outputs::first_available(available_encoders, crate::outputs::H264_ENCODERS).unwrap_or_else(|| "obs_x264".into());
    let aenc_id = crate::outputs::aac_encoder(available_encoders);
    let c = |s: &str| std::ffi::CString::new(s).unwrap_or_default();
    unsafe {
        let v = crate::obs::Data::from_json(&serde_json::json!({
            "bitrate": 6000, "rate_control": "CBR", "profile": "high", "keyint_sec": 1,
            "bframes": false, "x264opts": "bframes=0", "preset": "veryfast", "tune": "zerolatency",
        }));
        let video = sys::obs_video_encoder_create(c(&venc_id).as_ptr(), c"rtsp video".as_ptr(), v.as_ptr(), std::ptr::null_mut());
        let a = crate::obs::Data::from_json(&serde_json::json!({"bitrate": 160}));
        let audio = sys::obs_audio_encoder_create(c(&aenc_id).as_ptr(), c"rtsp audio".as_ptr(), a.as_ptr(), 0, std::ptr::null_mut());
        if video.is_null() || audio.is_null() {
            return Err("Couldn't make the encoders".into());
        }
        sys::obs_encoder_set_video(video, crate::outputs::primary_video());
        sys::obs_encoder_set_audio(audio, sys::obs_get_audio());
        let output = sys::obs_output_create(c"lightair_rtsp".as_ptr(), c"rtsp".as_ptr(), std::ptr::null_mut(), std::ptr::null_mut());
        if output.is_null() {
            sys::obs_encoder_release(video);
            sys::obs_encoder_release(audio);
            return Err("Couldn't start the RTSP output".into());
        }
        sys::obs_output_set_video_encoder(output, video);
        sys::obs_output_set_audio_encoder(output, audio, 0);
        if !sys::obs_output_start(output) {
            sys::obs_output_release(output);
            sys::obs_encoder_release(video);
            sys::obs_encoder_release(audio);
            return Err("The RTSP output didn't start".into());
        }
        *r = Some(Running::Libobs { output, video, audio });
    }
    tracing::info!(port = PORT, "serving the program over RTSP");
    Ok(())
}
