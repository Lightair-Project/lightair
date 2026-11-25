//! VST3 hosting on the main thread: moduleinfo.json reading, and (when Surge XT Effects is
//! installed) probing, loading, processing and state with a real plugin.

use std::path::PathBuf;

use lightair_audio::dsp::Plugin;
use lightair_obs::plugins::vst3::{self, Vst3Plugin};

fn module_info_without_loading() {
    let dir = std::env::temp_dir().join(format!("lightair-vst3-{}", std::process::id()));
    let res = dir.join("Fake.vst3/Contents/Resources");
    std::fs::create_dir_all(&res).unwrap();
    // as plugins ship it: JSON with trailing commas
    std::fs::write(
        res.join("moduleinfo.json"),
        r#"{
          "Name": "Fake", "Factory Info": { "Vendor": "Fake Co", },
          "Classes": [
            { "CID": "0123456789ABCDEF0123456789ABCDEF", "Category": "Audio Module Class",
              "Name": "Fake Comp, \"pro\"", "Vendor": "", "Version": "1.0", "Sub Categories": [ "Fx", "Dynamics", ], },
            { "CID": "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF", "Category": "Audio Module Class",
              "Name": "Fake Synth", "Sub Categories": [ "Instrument", ], },
            { "CID": "EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE", "Category": "Component Controller Class",
              "Name": "Fake Comp", },
          ],
        }"#,
    )
    .unwrap();
    let list = vst3::from_module_info(&dir.join("Fake.vst3")).expect("parses");
    assert_eq!(list.len(), 1, "the effect only: {list:?}");
    assert_eq!(list[0].name, "Fake Comp, \"pro\"");
    assert_eq!(list[0].vendor, "Fake Co");
    assert_eq!(list[0].id, "vst3:0123456789ABCDEF0123456789ABCDEF");
    let _ = std::fs::remove_dir_all(&dir);
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

/// Surge XT Effects, if installed: probe, load, process, state.
fn a_real_plugin() -> bool {
    let bundle = PathBuf::from("/Library/Audio/Plug-Ins/VST3/Surge XT Effects.vst3");
    if !bundle.exists() {
        println!("Surge XT Effects not installed: skipped");
        return false;
    }
    let list = vst3::probe(&bundle).expect("probes");
    let info = list.first().cloned().expect("an effect class");
    assert_eq!(info.format, "vst3");
    let p = Vst3Plugin::load(&info, 48_000.0).expect("loads");
    let mut out = 0.0;
    for b in 0..40 {
        let mut l: Vec<f32> =
            (0..256).map(|i| (std::f32::consts::TAU * 440.0 * (b * 256 + i) as f32 / 48_000.0).sin() * 0.5).collect();
        let mut r = l.clone();
        assert!(p.process(&mut l, &mut r), "processes");
        assert!(l.iter().chain(&r).all(|x| x.is_finite()));
        out = rms(&l);
    }
    assert!(out > 0.01, "audio comes out: {out}");
    let state = p.state().expect("has a state");
    let q = Vst3Plugin::load(&info, 48_000.0).expect("a second instance");
    assert!(q.set_state(&state), "takes the state");
    assert!(p.create_view().is_some(), "has an editor");
    drop(q);
    drop(p);
    true
}

fn main() {
    module_info_without_loading();
    let n = 1 + a_real_plugin() as usize;
    println!("test result: ok. {n} passed; 0 failed");
}
