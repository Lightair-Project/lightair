//! Loudness metering per EBU R128 / ITU-R BS.1770 (docs/10-audio.md, "Meters"): K-weighting,
//! momentary (400 ms), short-term (3 s) and gated integrated loudness, in LUFS. Pure DSP, and the
//! meter on the stream mix (the mixer's Program).

use std::collections::VecDeque;

/// One biquad section (direct form I).
#[derive(Clone, Copy, Default)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 3],
    x: [f64; 2],
    y: [f64; 2],
}

impl Biquad {
    fn run(&mut self, x0: f64) -> f64 {
        let y0 = self.b[0] * x0 + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[1] * self.y[0]
            - self.a[2] * self.y[1];
        self.x = [x0, self.x[0]];
        self.y = [y0, self.y[0]];
        y0
    }
}

/// The two K-weighting stages for a sample rate (coefficients as in BS.1770, derived for any rate).
fn k_weighting(rate: f64) -> [Biquad; 2] {
    use std::f64::consts::PI;
    // Stage 1: high shelf modelling the head.
    let (f0, g, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
    let k = (PI * f0 / rate).tan();
    let vh = 10f64.powf(g / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad {
        b: [(vh + vb * k / q + k * k) / a0, 2.0 * (k * k - vh) / a0, (vh - vb * k / q + k * k) / a0],
        a: [1.0, 2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        ..Default::default()
    };
    // Stage 2: the RLB high-pass.
    let (f0, q) = (38.13547087602444, 0.5003270373238773);
    let k = (PI * f0 / rate).tan();
    let a0 = 1.0 + k / q + k * k;
    let highpass = Biquad {
        b: [1.0, -2.0, 1.0],
        a: [1.0, 2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        ..Default::default()
    };
    [shelf, highpass]
}

fn lufs(power: f64) -> f64 {
    if power > 0.0 { -0.691 + 10.0 * power.log10() } else { f64::NEG_INFINITY }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoudnessReading {
    pub momentary: f64,
    pub short_term: f64,
    pub integrated: f64,
}

pub struct Loudness {
    filters: Vec<[Biquad; 2]>,
    /// Samples per 100 ms sub-block.
    sub_len: usize,
    sub_count: usize,
    sub_sum: f64,
    /// Mean-square power of the latest sub-blocks (channels summed), newest last; up to 3 s.
    subs: VecDeque<f64>,
    /// Power of every 400 ms block (75 % overlap) since the last reset, for integrated loudness.
    blocks: Vec<f64>,
}

impl Loudness {
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        Self {
            filters: (0..channels).map(|_| k_weighting(sample_rate as f64)).collect(),
            sub_len: (sample_rate as usize / 10).max(1),
            sub_count: 0,
            sub_sum: 0.0,
            subs: VecDeque::with_capacity(31),
            blocks: Vec::new(),
        }
    }

    /// Feeds planar audio: one slice per channel, all the same length.
    pub fn push(&mut self, planes: &[&[f32]]) {
        let frames = planes.iter().map(|p| p.len()).min().unwrap_or(0);
        for i in 0..frames {
            let mut sum = 0.0;
            for (ch, plane) in planes.iter().enumerate().take(self.filters.len()) {
                let [s1, s2] = &mut self.filters[ch];
                let y = s2.run(s1.run(plane[i] as f64));
                sum += y * y;
            }
            self.sub_sum += sum;
            self.sub_count += 1;
            if self.sub_count == self.sub_len {
                self.subs.push_back(self.sub_sum / self.sub_len as f64);
                if self.subs.len() > 30 {
                    self.subs.pop_front();
                }
                if self.subs.len() >= 4 {
                    let block = self.subs.iter().rev().take(4).sum::<f64>() / 4.0;
                    self.blocks.push(block);
                }
                self.sub_sum = 0.0;
                self.sub_count = 0;
            }
        }
    }

    fn mean_of_last(&self, n: usize) -> f64 {
        if self.subs.len() < n {
            return 0.0;
        }
        self.subs.iter().rev().take(n).sum::<f64>() / n as f64
    }

    /// Gated integrated loudness: absolute gate −70 LUFS, then relative gate −10 LU.
    fn integrated(&self) -> f64 {
        let above: Vec<f64> = self.blocks.iter().copied().filter(|p| lufs(*p) > -70.0).collect();
        if above.is_empty() {
            return f64::NEG_INFINITY;
        }
        let relative = lufs(above.iter().sum::<f64>() / above.len() as f64) - 10.0;
        let gated: Vec<f64> = above.into_iter().filter(|p| lufs(*p) > relative).collect();
        if gated.is_empty() {
            return f64::NEG_INFINITY;
        }
        lufs(gated.iter().sum::<f64>() / gated.len() as f64)
    }

    pub fn reading(&self) -> LoudnessReading {
        LoudnessReading {
            momentary: lufs(self.mean_of_last(4)),
            short_term: lufs(self.mean_of_last(30)),
            integrated: self.integrated(),
        }
    }

    /// Starts integrated loudness over (e.g. when a stream starts).
    pub fn reset_integrated(&mut self) {
        self.blocks.clear();
    }
}

// ---------------------------------------------------------------- the stream mix
static STREAM: parking_lot::Mutex<Option<Loudness>> = parking_lot::Mutex::new(None);

/// The mixer's Program, taken for the meter.
static TAP: parking_lot::Mutex<Option<crate::audio_host::MixTap>> = parking_lot::Mutex::new(None);

/// Starts metering the stream mix (Program, what the encoders send), straight from the mixer.
pub fn attach(sample_rate: u32, channels: usize) {
    *STREAM.lock() = Some(Loudness::new(sample_rate, channels));
    let mut left = Vec::with_capacity(crate::audio_host::BLOCK * 4);
    let mut right = Vec::with_capacity(crate::audio_host::BLOCK * 4);
    let tap = crate::audio_host::tap_mix(0, move |interleaved, _| {
        left.clear();
        right.clear();
        for frame in interleaved.as_chunks::<2>().0 {
            left.push(frame[0]);
            right.push(frame[1]);
        }
        if let Some(meter) = STREAM.lock().as_mut() {
            let planes: [&[f32]; 2] = [&left, &right];
            let n = meter.filters.len().min(2);
            meter.push(&planes[..n]);
        }
    });
    *TAP.lock() = Some(tap);
}

/// Starts metering sound something else pushes ([`push`]): libobs's mix.
pub fn attach_pushed(sample_rate: u32, channels: usize) {
    *STREAM.lock() = Some(Loudness::new(sample_rate, channels));
}

/// Planar sound of the stream mix, for the meter (never waits for the lock).
pub fn push(planes: &[&[f32]]) {
    let Some(mut guard) = STREAM.try_lock() else { return };
    if let Some(meter) = guard.as_mut() {
        let n = meter.filters.len().min(planes.len());
        meter.push(&planes[..n]);
    }
}

/// Stops metering; whether it was the mixer's tap (else the pushed sound).
pub fn detach() -> bool {
    let tap = TAP.lock().take();
    *STREAM.lock() = None;
    tap.is_some()
}

/// Loudness of what's being streamed and recorded.
pub fn stream_loudness() -> Option<LoudnessReading> {
    STREAM.lock().as_ref().map(Loudness::reading)
}

pub fn reset_stream_loudness() {
    if let Some(m) = STREAM.lock().as_mut() {
        m.reset_integrated();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, freq: f64, amp: f32, seconds: f64) -> Vec<f32> {
        let n = (rate as f64 * seconds) as usize;
        (0..n).map(|i| amp * (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() as f32).collect()
    }

    #[test]
    fn reference_tones() {
        // BS.1770 calibration: 1 kHz at −20 dBFS on one channel reads −23 LUFS.
        let mut m = Loudness::new(48_000, 2);
        let tone = sine(48_000, 997.0, 0.1, 10.0);
        let silence = vec![0.0f32; tone.len()];
        m.push(&[&tone, &silence]);
        let r = m.reading();
        assert!((r.integrated + 23.0).abs() < 0.1, "{r:?}");
        assert!((r.momentary + 23.0).abs() < 0.1 && (r.short_term + 23.0).abs() < 0.1, "{r:?}");

        // Both channels: +3 dB.
        let mut m = Loudness::new(44_100, 2);
        let tone = sine(44_100, 997.0, 0.1, 5.0);
        m.push(&[&tone, &tone]);
        assert!((m.reading().integrated + 20.0).abs() < 0.1, "{:?}", m.reading());
    }

    #[test]
    fn gating_ignores_silence_and_quiet_parts() {
        let mut m = Loudness::new(48_000, 1);
        let loud = sine(48_000, 997.0, 0.1, 5.0);
        let quiet = sine(48_000, 997.0, 0.001, 5.0); // −63 LUFS: below the relative gate
        let silent = vec![0.0f32; 48_000 * 5];
        for part in [&loud, &silent, &quiet, &loud] {
            m.push(&[part]);
        }
        assert!((m.reading().integrated + 23.0).abs() < 0.2, "{:?}", m.reading());
        assert_eq!(Loudness::new(48_000, 1).reading().integrated, f64::NEG_INFINITY);
        m.reset_integrated();
        assert_eq!(m.reading().integrated, f64::NEG_INFINITY);
    }
}
