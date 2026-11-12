//! From the project's mixer to the audio engine's graph (docs/plans/audio-engine.md): tracks,
//! buses, mixes and the master, with each effect's settings read through the catalog (missing
//! values take the catalog's defaults).

use lightair_audio as la;
use la::dsp::*;
use lightair_core::catalog::{self, EffectDef};
use lightair_core::{Effect, Id, Project, Route, TrackChannels};
use serde_json::Value;

/// The engine's id for a document id.
pub fn node(id: Id) -> la::NodeId {
    id.0.as_u128()
}

struct Reader<'a> {
    def: &'static EffectDef,
    s: &'a Value,
}

impl Reader<'_> {
    fn default(&self, key: &str) -> Option<Value> {
        self.def.controls.iter().find(|c| c.key() == Some(key)).and_then(|c| c.default_json())
    }
    fn num(&self, key: &str) -> f32 {
        self.s.get(key).and_then(Value::as_f64).or_else(|| self.default(key).and_then(|v| v.as_f64())).unwrap_or(0.0)
            as f32
    }
    fn flag(&self, key: &str) -> bool {
        self.s.get(key).and_then(Value::as_bool).or_else(|| self.default(key).and_then(|v| v.as_bool())).unwrap_or(false)
    }
    fn text(&self, key: &str) -> String {
        self.s
            .get(key)
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| self.default(key).and_then(|v| v.as_str().map(String::from)))
            .unwrap_or_default()
    }
}

fn band(r: &Reader, n: usize) -> Band {
    let k = |f: &str| format!("b{n}_{f}");
    let slope = r.num(&k("slope")).clamp(12.0, 48.0) as u8;
    let kind = match r.text(&k("type")).as_str() {
        "hp" => BandKind::HighPass(slope),
        "lp" => BandKind::LowPass(slope),
        "ls" => BandKind::LowShelf,
        "hs" => BandKind::HighShelf,
        "notch" => BandKind::Notch,
        _ => BandKind::Bell,
    };
    Band { on: r.flag(&k("on")), kind, freq: r.num(&k("freq")), gain_db: r.num(&k("gain")), q: r.num(&k("q")) }
}

/// An effect's engine params; `None` for kinds the engine doesn't run (older libobs filters).
pub fn params(e: &Effect) -> Option<Params> {
    let def = catalog::def(&e.kind)?;
    let r = Reader { def, s: &e.settings };
    Some(match e.kind.as_str() {
        "audio.gain" => Params::Gain(GainParams {
            db: r.num("db"),
            invert: r.flag("invert"),
            mono: r.flag("mono"),
            swap: r.flag("swap"),
        }),
        "audio.eq" => {
            let mut p = EqParams::default();
            for (i, b) in p.bands.iter_mut().enumerate() {
                *b = band(&r, i + 1);
            }
            p.output_db = r.num("output_db");
            Params::Eq(p)
        }
        "audio.compressor" => Params::Compressor(CompressorParams {
            threshold_db: r.num("threshold_db"),
            ratio: r.num("ratio"),
            knee_db: r.num("knee_db"),
            attack_ms: r.num("attack_ms"),
            release_ms: r.num("release_ms"),
            makeup_db: r.num("makeup_db"),
            auto_makeup: r.flag("auto_makeup"),
            mix: r.num("mix"),
            sidechain_hpf: r.num("sidechain_hpf"),
        }),
        "audio.gate" => Params::Gate(GateParams {
            threshold_db: r.num("threshold_db"),
            range_db: r.num("range_db"),
            ratio: r.num("ratio"),
            attack_ms: r.num("attack_ms"),
            hold_ms: r.num("hold_ms"),
            release_ms: r.num("release_ms"),
            hysteresis_db: r.num("hysteresis_db"),
        }),
        "audio.limiter" => Params::Limiter(LimiterParams {
            ceiling_db: r.num("ceiling_db"),
            release_ms: r.num("release_ms"),
            input_db: r.num("input_db"),
        }),
        "audio.deesser" => Params::DeEsser(DeEsserParams {
            freq: r.num("freq"),
            threshold_db: r.num("threshold_db"),
            range_db: r.num("range_db"),
            split: r.flag("split"),
            listen: r.flag("listen"),
        }),
        "audio.ducker" => Params::Ducker(DuckerParams {
            source: r.text("source").parse::<uuid::Uuid>().map(|u| u.as_u128()).unwrap_or(0),
            amount_db: r.num("amount_db"),
            threshold_db: r.num("threshold_db"),
            attack_ms: r.num("attack_ms"),
            hold_ms: r.num("hold_ms"),
            release_ms: r.num("release_ms"),
        }),
        "audio.noise" => Params::Noise(NoiseParams { strength: r.num("strength"), vad_threshold: r.num("vad_threshold") }),
        "audio.reverb" => Params::Reverb(ReverbParams {
            size: r.num("size"),
            damping: r.num("damping"),
            pre_delay_ms: r.num("pre_delay_ms"),
            width: r.num("width"),
            mix: r.num("mix"),
        }),
        "audio.delay" => Params::Delay(DelayParams {
            time_ms: r.num("time_ms"),
            feedback: r.num("feedback"),
            tone_hz: r.num("tone_hz"),
            ping_pong: r.flag("ping_pong"),
            mix: r.num("mix"),
        }),
        "audio.channel" => Params::Channel(ChannelParams { width: r.num("width"), bass_mono_hz: r.num("bass_mono_hz") }),
        "audio.voice" => Params::Voice(VoiceParams {
            amount: r.num("amount"),
            noise: r.flag("noise"),
            gate: r.flag("gate"),
            deess: r.flag("deess"),
            eq: r.flag("eq"),
            compress: r.flag("compress"),
            limit: r.flag("limit"),
        }),
        _ => return None,
    })
}

fn inserts(effects: &[Effect]) -> Vec<la::EffectSpec> {
    effects
        .iter()
        .filter_map(|e| params(e).map(|p| la::EffectSpec { id: node(e.id), enabled: e.enabled, params: p }))
        .collect()
}

fn dest(r: Route) -> la::Dest {
    match r {
        Route::Program => la::Dest::Program,
        Route::Bus(b) => la::Dest::Bus(node(b)),
        Route::None => la::Dest::Nowhere,
    }
}

fn sends(sends: &[lightair_core::Send]) -> Vec<la::Send> {
    sends.iter().map(|s| la::Send { mix: node(s.mix), level_db: s.level_db, pre_fader: s.pre_fader }).collect()
}

fn fader(db: f32) -> f32 {
    if db.is_finite() { db } else { -145.0 }
}

/// The engine graph for a project's mixer.
pub fn spec(project: &Project) -> la::GraphSpec {
    let mut strips: Vec<la::Strip> = project
        .tracks
        .iter()
        .map(|t| {
            let channels = match t.channels {
                // the input layer delivers mono inputs on both channels
                TrackChannels::All => la::Channels::Stereo(0, 1),
                TrackChannels::Mono(c) => la::Channels::Mono(c),
                TrackChannels::Stereo(a, b) => la::Channels::Stereo(a, b),
            };
            la::Strip {
                name: t.name.clone(),
                trim_db: t.trim_db,
                invert: t.invert,
                mono: t.mono,
                inserts: inserts(&t.effects),
                gain_db: fader(t.volume_db),
                pan: t.pan,
                mute: t.muted,
                solo: false,
                dest: dest(t.route),
                sends: sends(&t.sends),
                ..la::Strip::track(node(t.id), node(t.input), channels)
            }
        })
        .collect();
    strips.extend(project.audio.buses.iter().map(|b| la::Strip {
        name: b.name.clone(),
        inserts: inserts(&b.effects),
        gain_db: fader(b.volume_db),
        pan: b.pan,
        mute: b.muted,
        dest: dest(b.route),
        sends: sends(&b.sends),
        ..la::Strip::bus(node(b.id))
    }));
    let master = &project.audio.master;
    la::GraphSpec {
        strips,
        mixes: project
            .audio
            .mixes
            .iter()
            .map(|m| la::Mix {
                name: m.name.clone(),
                inserts: inserts(&m.effects),
                gain_db: fader(m.volume_db),
                mute: m.muted,
                ..la::Mix::new(node(m.id))
            })
            .collect(),
        program: la::Master { inserts: inserts(&master.effects), gain_db: fader(master.volume_db), mute: master.muted },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightair_core::{AudioMix, Bus, Input, Send, Track};
    use serde_json::json;

    #[test]
    fn a_project_mixer_becomes_the_engine_graph() {
        let mut p = Project::new("Show");
        let mic = Input::new("Mic", "coreaudio_input_capture", json!({}));
        let bus = Bus::new("Guests");
        let phones = AudioMix::new("Headphones");
        let mut t = Track::new("Host", mic.id);
        t.channels = TrackChannels::Mono(2);
        t.route = Route::Bus(bus.id);
        t.sends = vec![Send { mix: phones.id, level_db: -6.0, pre_fader: true }];
        t.volume_db = f32::NEG_INFINITY;
        t.effects = vec![
            Effect::new("Voice", "audio.voice", json!({"amount": 0.8})),
            Effect::new("Old", "noise_suppress_filter_v2", json!({})),
        ];
        let tid = t.id;
        p.inputs.insert(mic.id, mic.into());
        p.tracks.push(t.into());
        p.audio.buses.push(bus.clone());
        p.audio.mixes.push(phones.clone());
        let g = spec(&p);
        let host = g.strips.iter().find(|s| s.id == node(tid)).unwrap();
        assert_eq!(host.role, la::Role::Track { input: node(p.tracks[0].input), channels: la::Channels::Mono(2) });
        assert_eq!(host.dest, la::Dest::Bus(node(bus.id)));
        assert_eq!(host.sends, vec![la::Send { mix: node(phones.id), level_db: -6.0, pre_fader: true }]);
        assert!(host.gain_db < -144.0, "−∞ fader: silence");
        assert_eq!(host.inserts.len(), 1, "the old libobs filter isn't the engine's");
        match &host.inserts[0].params {
            Params::Voice(v) => assert!((v.amount - 0.8).abs() < 1e-6 && v.noise, "set value + defaults"),
            other => panic!("{other:?}"),
        }
        assert!(g.strips.iter().any(|s| s.id == node(bus.id) && s.role == la::Role::Bus));
        assert_eq!(g.mixes.len(), 1);
    }

    #[test]
    fn eq_bands_and_a_ducker_source_read_from_settings() {
        let src = Id::new();
        let eq = Effect::new("EQ", "audio.eq", json!({"b1_on": true, "b1_slope": 48, "b4_on": true, "b4_gain": 3.0}));
        let Some(Params::Eq(p)) = params(&eq) else { panic!() };
        assert_eq!(p.bands[0].kind, BandKind::HighPass(48));
        assert!(p.bands[0].on && (p.bands[0].freq - 80.0).abs() < 1e-3);
        assert_eq!(p.bands[3].kind, BandKind::Bell);
        assert!((p.bands[3].gain_db - 3.0).abs() < 1e-6 && !p.bands[1].on);
        let duck = Effect::new("Duck", "audio.ducker", json!({"source": src.0.to_string()}));
        let Some(Params::Ducker(d)) = params(&duck) else { panic!() };
        assert_eq!(d.source, node(src));
        assert!((d.amount_db - 12.0).abs() < 1e-6);
    }
}
