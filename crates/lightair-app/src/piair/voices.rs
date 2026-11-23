//! The co-host's voices: what each vendor offers (an account's voices for ElevenLabs and
//! Cartesia, Gemini's prebuilt ones) and a short preview of one, played through this Mac's own
//! output (never the stream). Network and Keychain work: off the main thread.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;

use base64::Engine as _;
use serde_json::{Value, json};

use super::keys;

/// A voice the co-host can use.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceInfo {
    pub id: String,
    pub name: String,
    /// A few words about it ("American, young, warm").
    pub detail: String,
    /// Those words one by one (accent, age, style, what it's for), for the picker's tags.
    pub tags: Vec<String>,
    /// "female", "male" or "neutral", when the vendor says.
    pub gender: Option<String>,
    /// The vendor's own sample, when it has one.
    pub preview_url: Option<String>,
}

/// What the preview says.
const SAMPLE: &str = "Hey chat! I'm your co-host. Ask me anything, I'm listening.";

/// Gemini's prebuilt voices and their character (from Google's speech generation guide).
const GEMINI: &[(&str, &str)] = &[
    ("Puck", "Upbeat"),
    ("Zephyr", "Bright"),
    ("Charon", "Informative"),
    ("Kore", "Firm"),
    ("Fenrir", "Excitable"),
    ("Leda", "Youthful"),
    ("Orus", "Firm"),
    ("Aoede", "Breezy"),
    ("Callirrhoe", "Easy-going"),
    ("Autonoe", "Bright"),
    ("Enceladus", "Breathy"),
    ("Iapetus", "Clear"),
    ("Umbriel", "Easy-going"),
    ("Algieba", "Smooth"),
    ("Despina", "Smooth"),
    ("Erinome", "Clear"),
    ("Algenib", "Gravelly"),
    ("Rasalgethi", "Informative"),
    ("Laomedeia", "Upbeat"),
    ("Achernar", "Soft"),
    ("Alnilam", "Firm"),
    ("Schedar", "Even"),
    ("Gacrux", "Mature"),
    ("Pulcherrima", "Forward"),
    ("Achird", "Friendly"),
    ("Zubenelgenubi", "Casual"),
    ("Vindemiatrix", "Gentle"),
    ("Sadachbia", "Lively"),
    ("Sadaltager", "Knowledgeable"),
    ("Sulafat", "Warm"),
];

const CARTESIA_VERSION: &str = "2026-08-14";

fn key_for(vendor: &str) -> Result<String, String> {
    let service = keys::service_for_voice(vendor);
    keys::get(service).ok_or_else(|| format!("No {} key: add one in Settings → Connections", keys::name_of(service)))
}

fn json_get(url: &str, headers: &[(&str, &str)]) -> Result<Value, String> {
    let agent = lightair_live::net::agent();
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut res = req.call().map_err(|e| format!("Couldn't reach it: {e}"))?;
    let status = res.status().as_u16();
    let body = res.body_mut().read_to_string().unwrap_or_default();
    match status {
        200..=299 => serde_json::from_str(&body).map_err(|e| e.to_string()),
        401 | 403 if body.contains("missing_permissions") => {
            Err("The key can't list voices: give it Voices (read)".into())
        }
        401 | 403 => Err("The key wasn't accepted".into()),
        s => Err(format!("The service answered {s}")),
    }
}

/// The voices `vendor` offers this account.
pub fn list(vendor: &str) -> Result<Vec<VoiceInfo>, String> {
    match vendor {
        "gemini" => Ok(GEMINI
            .iter()
            .map(|(n, d)| VoiceInfo {
                id: (*n).into(),
                name: (*n).into(),
                detail: (*d).into(),
                tags: vec![(*d).into()],
                gender: None,
                preview_url: None,
            })
            .collect()),
        "elevenlabs" => {
            let key = key_for(vendor)?;
            let v = json_get("https://api.elevenlabs.io/v2/voices?page_size=100", &[("xi-api-key", &key)])?;
            Ok(v["voices"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| {
                            let l = &x["labels"];
                            let word =
                                |k: &str| l[k].as_str().map(|v| cap(&v.replace('_', " "))).filter(|v| !v.is_empty());
                            let mut tags: Vec<String> = ["accent", "age", "descriptive", "description", "use_case"]
                                .iter()
                                .filter_map(|k| word(k))
                                .collect();
                            tags.dedup();
                            if matches!(x["category"].as_str(), Some("cloned" | "generated" | "professional")) {
                                tags.insert(0, "Yours".into());
                            }
                            let gender = l["gender"].as_str().map(|g| g.to_lowercase());
                            Some(VoiceInfo {
                                id: x["voice_id"].as_str()?.into(),
                                name: x["name"].as_str()?.split(" - ").next().unwrap_or_default().trim().into(),
                                detail: tags.join(", "),
                                tags,
                                gender,
                                preview_url: x["preview_url"]
                                    .as_str()
                                    .filter(|u| u.starts_with("https://"))
                                    .map(str::to_owned),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default())
        }
        "cartesia" => {
            let key = key_for(vendor)?;
            let v = json_get(
                "https://api.cartesia.ai/voices?limit=100",
                &[("X-API-Key", &key), ("Cartesia-Version", CARTESIA_VERSION)],
            )?;
            let list = if v.is_array() { v } else { v["data"].clone() };
            Ok(list
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| {
                            let mut tags: Vec<String> = Vec::new();
                            if let Some(lang) = x["language"].as_str() {
                                tags.push(lang.to_uppercase());
                            }
                            if let Some(d) = x["description"].as_str() {
                                // the description's first few words
                                let short: String = d.split_whitespace().take(5).collect::<Vec<_>>().join(" ");
                                tags.push(short.trim_end_matches(['.', ',']).to_string());
                            }
                            let gender = x["gender"].as_str().map(|g| match g {
                                "feminine" | "female" => "female".to_string(),
                                "masculine" | "male" => "male".to_string(),
                                _ => "neutral".to_string(),
                            });
                            Some(VoiceInfo {
                                id: x["id"].as_str()?.into(),
                                name: x["name"].as_str()?.into(),
                                detail: tags.join(", "),
                                tags,
                                gender,
                                preview_url: None,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default())
        }
        _ => Ok(Vec::new()),
    }
}

/// A preview of `voice`, as a file (the vendor's sample, or a sentence spoken in it), kept for
/// next time.
pub fn preview(vendor: &str, voice: &VoiceInfo) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("lightair-voice-previews");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let safe: String = voice.id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let ext = if voice.preview_url.is_some() || vendor == "elevenlabs" { "mp3" } else { "wav" };
    let file = dir.join(format!("{vendor}-{safe}.{ext}"));
    if file.exists() {
        return Ok(file);
    }
    let agent = lightair_live::net::agent();
    let bytes: Vec<u8> = if let Some(url) = &voice.preview_url {
        let mut res = agent.get(url).call().map_err(|e| e.to_string())?;
        res.body_mut().read_to_vec().map_err(|e| e.to_string())?
    } else {
        let key = key_for(vendor)?;
        match vendor {
            "elevenlabs" => {
                let url =
                    format!("https://api.elevenlabs.io/v1/text-to-speech/{}?output_format=mp3_44100_128", voice.id);
                let mut res = agent
                    .post(&url)
                    .header("xi-api-key", &key)
                    .header("Content-Type", "application/json")
                    .send(json!({"text": SAMPLE, "model_id": "eleven_flash_v2_5"}).to_string())
                    .map_err(|e| e.to_string())?;
                let status = res.status().as_u16();
                if !(200..300).contains(&status) {
                    return Err(format!("ElevenLabs answered {status}"));
                }
                res.body_mut().read_to_vec().map_err(|e| e.to_string())?
            }
            "cartesia" => {
                let mut res = agent
                    .post("https://api.cartesia.ai/tts/bytes")
                    .header("X-API-Key", &key)
                    .header("Cartesia-Version", CARTESIA_VERSION)
                    .header("Content-Type", "application/json")
                    .send(
                        json!({
                            "model_id": "sonic-3.6",
                            "transcript": SAMPLE,
                            "voice": {"mode": "id", "id": voice.id},
                            "output_format": {"container": "wav", "encoding": "pcm_s16le", "sample_rate": 24000},
                        })
                        .to_string(),
                    )
                    .map_err(|e| e.to_string())?;
                let status = res.status().as_u16();
                if !(200..300).contains(&status) {
                    return Err(format!("Cartesia answered {status}"));
                }
                res.body_mut().read_to_vec().map_err(|e| e.to_string())?
            }
            _ => gemini_sample(&key, &voice.id)?,
        }
    };
    std::fs::write(&file, &bytes).map_err(|e| e.to_string())?;
    Ok(file)
}

/// A sentence in a Gemini voice, as a WAV (its TTS gives raw 24 kHz PCM); the next model when
/// one is over the free tier's quota.
fn gemini_sample(key: &str, voice: &str) -> Result<Vec<u8>, String> {
    let agent = lightair_live::net::agent();
    let body = json!({
        "contents": [{"parts": [{"text": SAMPLE}]}],
        "generationConfig": {"responseModalities": ["AUDIO"], "speechConfig": {"voiceConfig": {"prebuiltVoiceConfig": {"voiceName": voice}}}},
    })
    .to_string();
    let mut last = String::new();
    for model in ["gemini-3.8-flash-tts", "gemini-3.1-flash-tts-preview", "gemini-2.5-flash-preview-tts"] {
        let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent");
        let mut res = agent
            .post(&url)
            .header("x-goog-api-key", key)
            .header("Content-Type", "application/json")
            .send(body.clone())
            .map_err(|e| e.to_string())?;
        let status = res.status().as_u16();
        let text = res.body_mut().read_to_string().unwrap_or_default();
        if status == 429 || status == 404 {
            last = format!("{model} answered {status}");
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(format!("Gemini answered {status}"));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let data =
            v["candidates"][0]["content"]["parts"][0]["inlineData"]["data"].as_str().ok_or("Gemini sent no audio")?;
        let pcm = base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| e.to_string())?;
        return Ok(wav(&pcm, 24_000));
    }
    Err(format!("Every Gemini voice model is over its quota for today ({last})"))
}

/// 16-bit mono PCM in a WAV file.
fn wav(pcm: &[u8], rate: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

static PLAYING: Mutex<Option<Child>> = Mutex::new(None);

/// A preview is playing.
pub fn playing() -> bool {
    let mut g = PLAYING.lock().unwrap_or_else(|e| e.into_inner());
    match g.as_mut() {
        Some(c) => match c.try_wait() {
            Ok(None) => true,
            _ => {
                *g = None;
                false
            }
        },
        None => false,
    }
}

/// Stops the preview playing.
pub fn stop() {
    if let Some(mut c) = PLAYING.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Plays a preview through this computer's own output (stopping the one before).
pub fn play(file: &std::path::Path) {
    let mut g = PLAYING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mut c) = g.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    #[cfg(target_os = "macos")]
    let child = Command::new("/usr/bin/afplay").arg(file).spawn();
    #[cfg(windows)]
    let child = Command::new("powershell")
        .args(["-NoProfile", "-Command", &format!("Add-Type -AssemblyName presentationCore; $p = New-Object System.Windows.Media.MediaPlayer; $p.Open('{}'); $p.Play(); Start-Sleep 12", file.display())])
        .spawn();
    if let Ok(c) = child {
        *g = Some(c);
    }
}
