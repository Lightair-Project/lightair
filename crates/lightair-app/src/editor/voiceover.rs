//! Voice-over: the microphone recorded while the project plays (its picture; its sound off, so
//! it isn't recorded again), then placed where it began on an audio track.

use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::*;

/// The microphone being recorded.
pub(super) struct Recorder {
    _stream: cpal::Stream,
    samples: Arc<Mutex<Vec<f32>>>,
    rate: u32,
    channels: usize,
    /// Where on the timeline it began.
    pub at: u64,
    pub began: std::time::Instant,
}

impl Recorder {
    fn start(at: u64) -> Result<Recorder, String> {
        let device = cpal::default_host().default_input_device().ok_or("no microphone")?;
        let config = device.default_input_config().map_err(|e| e.to_string())?;
        let (rate, channels) = (config.sample_rate().0, config.channels() as usize);
        let samples = Arc::new(Mutex::new(Vec::with_capacity(rate as usize * channels * 60)));
        let into = samples.clone();
        let err = |e: cpal::StreamError| tracing::warn!(%e, "voice-over");
        let stream = match config.sample_format() {
            cpal::SampleFormat::I16 => device.build_input_stream(&config.into(), move |d: &[i16], _| into.lock().unwrap().extend(d.iter().map(|s| *s as f32 / 32768.0)), err, None),
            _ => device.build_input_stream(&config.into(), move |d: &[f32], _| into.lock().unwrap().extend_from_slice(d), err, None),
        }
        .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(Recorder { _stream: stream, samples, rate, channels, at, began: std::time::Instant::now() })
    }

    /// Stops; the sound as stereo, and its rate.
    fn finish(self) -> (Vec<f32>, u32) {
        let (rate, ch) = (self.rate, self.channels.max(1));
        drop(self._stream);
        let raw = std::mem::take(&mut *self.samples.lock().unwrap());
        let stereo = raw.chunks(ch).flat_map(|f| if ch == 1 { [f[0], f[0]] } else { [f[0], f[1]] }).collect();
        (stereo, rate)
    }
}

impl Editor {
    /// Starts recording a voice-over at the playhead, or stops and places it.
    pub(super) fn voice_over(&mut self, cx: &mut Context<Self>) {
        if let Some(rec) = self.recorder.take() {
            self.stop(cx);
            let at = rec.at;
            let (stereo, rate) = rec.finish();
            if stereo.len() < rate as usize / 2 {
                self.note = Some("The voice-over was too short to keep.".into());
                cx.notify();
                return;
            }
            let folder = self.studio.read(cx).recording_folder();
            let name = format!("Voice-over {}", chrono::Local::now().format("%Y-%m-%d %H.%M.%S"));
            let path = folder.join(format!("{name}.wav"));
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let r = lightair_render::sound::write_wav(&path, &stereo, rate).map_err(|e| e.to_string()).and_then(|_| Media::from_file(&path).map_err(|e| e.to_string()));
                let _ = tx.send(match r {
                    Ok(m) => Done::Media(vec![Part::of(m)], Put::At(at), None),
                    Err(e) => Done::Exported(Err(format!("the voice-over couldn't be saved: {e}"))),
                });
            });
            self.note = Some("Voice-over recorded.".into());
            cx.notify();
            return;
        }
        if self.project.is_none() {
            return;
        }
        self.stop(cx);
        match Recorder::start(self.playhead) {
            Ok(r) => {
                self.recorder = Some(r);
                self.note = Some("Recording a voice-over: click again to stop.".into());
                // the picture plays to speak along to (its sound off)
                self.play_with(false, cx);
            }
            Err(e) => self.note = Some(format!("The microphone couldn't be opened: {e}").into()),
        }
        cx.notify();
    }
}
