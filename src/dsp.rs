//! The effects processing itself: low cut, noise gate, parametric EQ and compressor.
//! Plain maths on sample buffers, no PipeWire here, so it can be tested directly.
//!
//! The effects sit after the channel's fader, but gate and compressor judge the level
//! *before* the fader (they are told the fader position). So moving a fader never
//! changes when the gate opens or how hard the compressor works, as on a mixing desk.
//!
//! `Chain::set`, `Chain::set_rate` and `Chain::process` run in PipeWire's real-time
//! thread: they never allocate. Room for the maximum number of EQ points is reserved.

use crate::config::{
    BandKind, Comp, DeEsser, EQ_MAX_BANDS, Effects, EqBand, Gate, Limiter, NoiseSuppression,
};

/// Ducking settings as plain values (the source list lives in the config only).
#[derive(Clone, Copy, Debug, Default)]
struct DuckParams {
    enabled: bool,
    amount_db: f64,
    threshold_db: f64,
    attack_ms: f32,
    hold_ms: f32,
    release_ms: f32,
}
use nnnoiseless::{DenoiseState, RnnModel};
use std::sync::LazyLock;

/// Closes a little below where it opens, so a level hovering at the threshold doesn't
/// make it chatter.
const GATE_HYSTERESIS_DB: f64 = 3.0;
/// Most channels processed together (7.1). Extra channels pass through untouched.
pub const MAX_CHANNELS: usize = 8;
/// Longest delay, and the sample rate up to which it's available in full.
pub const MAX_DELAY_MS: f32 = 1000.0;
const DELAY_ROOM: usize = 96_000;
/// The limiter looks this far ahead, so it can turn down smoothly before a peak.
const LIMITER_LOOKAHEAD_S: f64 = 0.0015;
/// Room for the look-ahead at any sample rate up to 384 kHz.
const LOOKAHEAD_MAX: usize = 1024;
/// Noise suppression works on blocks of this many samples (10 ms at 48 kHz).
pub const NOISE_BLOCK: usize = DenoiseState::FRAME_SIZE;
/// The neural network's weights, shared by every channel.
static NOISE_MODEL: LazyLock<RnnModel> = LazyLock::new(RnnModel::default);

/// Noise suppression for one audio channel: collects a block, cleans it, and plays the
/// previous block back meanwhile (so exactly one block of delay).
struct Denoiser {
    state: Box<DenoiseState<'static>>,
    input: [f32; NOISE_BLOCK],
    /// Last block, cleaned and as it came in, for mixing.
    wet: [f32; NOISE_BLOCK],
    dry: [f32; NOISE_BLOCK],
}

impl Denoiser {
    fn new() -> Box<Self> {
        Box::new(Denoiser {
            state: DenoiseState::with_model(&NOISE_MODEL),
            input: [0.0; NOISE_BLOCK],
            wet: [0.0; NOISE_BLOCK],
            dry: [0.0; NOISE_BLOCK],
        })
    }
    fn clear(&mut self) {
        self.input.fill(0.0);
        self.wet.fill(0.0);
        self.dry.fill(0.0);
    }
}

/// One second-order filter (the "RBJ cookbook" designs used throughout audio software).
#[derive(Clone, Copy, Debug, Default)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    // Transposed direct form II state.
    z1: f64,
    z2: f64,
}

enum Shape {
    HighPass,
    LowPass,
    LowShelf { q: f64 },
    Bell { q: f64 },
    HighShelf { q: f64 },
}

impl Biquad {
    fn designed(shape: Shape, freq: f64, gain_db: f64, rate: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * (freq / rate).min(0.49);
        let (sin, cos) = w0.sin_cos();
        let (b0, b1, b2, a0, a1, a2) = match shape {
            Shape::HighPass => {
                let alpha = sin / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
                let c = (1.0 + cos) / 2.0;
                (c, -(1.0 + cos), c, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            Shape::LowPass => {
                let alpha = sin / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
                let c = (1.0 - cos) / 2.0;
                (c, 1.0 - cos, c, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            Shape::Bell { q } => {
                let alpha = sin / (2.0 * q);
                (
                    1.0 + alpha * a,
                    -2.0 * cos,
                    1.0 - alpha * a,
                    1.0 + alpha / a,
                    -2.0 * cos,
                    1.0 - alpha / a,
                )
            }
            Shape::LowShelf { q } | Shape::HighShelf { q } => {
                let alpha = sin / (2.0 * q);
                let k = 2.0 * a.sqrt() * alpha;
                let s = if matches!(shape, Shape::LowShelf { .. }) {
                    1.0
                } else {
                    -1.0
                };
                (
                    a * ((a + 1.0) - s * (a - 1.0) * cos + k),
                    s * 2.0 * a * ((a - 1.0) - s * (a + 1.0) * cos),
                    a * ((a + 1.0) - s * (a - 1.0) * cos - k),
                    (a + 1.0) + s * (a - 1.0) * cos + k,
                    -s * 2.0 * ((a - 1.0) + s * (a + 1.0) * cos),
                    (a + 1.0) + s * (a - 1.0) * cos - k,
                )
            }
        };
        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn low_cut(hz: f32, rate: f64) -> Self {
        Self::designed(Shape::HighPass, hz.clamp(20.0, 500.0) as f64, 0.0, rate)
    }

    fn band(b: &EqBand, rate: f64) -> Self {
        let q = (b.q as f64).clamp(0.1, 20.0);
        let shape = match b.kind {
            BandKind::LowShelf => Shape::LowShelf { q },
            BandKind::Bell => Shape::Bell { q },
            BandKind::HighShelf => Shape::HighShelf { q },
        };
        Self::designed(
            shape,
            (b.freq as f64).clamp(10.0, 22000.0),
            (b.gain_db as f64).clamp(-24.0, 24.0),
            rate,
        )
    }

    /// Take another filter's coefficients but keep this one's running state, so a
    /// change of settings doesn't click.
    fn take_coefficients(&mut self, other: &Biquad) {
        (self.b0, self.b1, self.b2, self.a1, self.a2) =
            (other.b0, other.b1, other.b2, other.a1, other.a2);
    }

    /// Gain in dB at `freq` (for drawing the EQ curve).
    fn response_db(&self, freq: f64, rate: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * freq / rate;
        let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
        let num_re = self.b0 + self.b1 * c1 + self.b2 * c2;
        let num_im = -(self.b1 * s1 + self.b2 * s2);
        let den_re = 1.0 + self.a1 * c1 + self.a2 * c2;
        let den_im = -(self.a1 * s1 + self.a2 * s2);
        10.0 * ((num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im)).log10()
    }

    #[inline]
    fn run(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// The combined gain (dB) of the low cut and EQ at `freq`, exactly as applied to the
/// audio. Used to draw the EQ curve.
pub fn response_db(fx: &Effects, freq: f64) -> f64 {
    const RATE: f64 = 48000.0;
    let mut total = 0.0;
    if fx.low_cut {
        total += Biquad::low_cut(fx.low_cut_hz, RATE).response_db(freq, RATE);
    }
    if fx.eq.enabled {
        total += fx
            .eq
            .bands
            .iter()
            .take(EQ_MAX_BANDS)
            .map(|b| Biquad::band(b, RATE).response_db(freq, RATE))
            .sum::<f64>();
    }
    total
}

/// What the effects did during the last block, for the GUI.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    /// Peak input level before the fader, dBFS.
    pub level_db: f32,
    pub gate_open: bool,
    /// How much the compressor turned the level down at most (dB, ≥ 0).
    pub reduction_db: f32,
    /// How much the limiter turned the level down at most (dB, ≥ 0).
    pub limit_db: f32,
    /// How much ducking turned the channel down (dB, ≥ 0).
    pub duck_db: f32,
    /// Loudest level of the ducking sources (dBFS).
    pub duck_source_db: f32,
    /// How much the de-esser turned the sharp sounds down at most (dB, ≥ 0).
    pub deess_db: f32,
}

/// The effects for one channel (up to MAX_CHANNELS audio channels, detection linked
/// across them so the stereo/surround image doesn't wander).
pub struct Chain {
    rate: f64,
    // The settings as plain values, so they can be copied in without allocating.
    low_cut: bool,
    low_cut_hz: f32,
    gate: Gate,
    comp: Comp,
    limiter: Limiter,
    noise: NoiseSuppression,
    deess: DeEsser,
    duck: DuckParams,
    mono: bool,
    eq_enabled: bool,
    bands: [EqBand; EQ_MAX_BANDS],
    band_count: usize,
    // Filters per audio channel.
    low_cut_filters: [Biquad; MAX_CHANNELS],
    eq_filters: [[Biquad; EQ_MAX_BANDS]; MAX_CHANNELS],
    /// De-esser: splits each channel at its frequency (the part above = input minus the
    /// part below, so with no reduction the sound is exactly unchanged).
    deess_filters: [Biquad; MAX_CHANNELS],
    /// What it listens to: a proper high-pass, so loud low sounds don't set it off.
    deess_detect: [Biquad; MAX_CHANNELS],
    deess_env: f64,
    /// Gate gain in dB (0 = open, -range = closed). Moving in dB at a steady rate gives
    /// the natural fade real gates have.
    gate_gain_db: f64,
    gate_hold: usize,
    gate_open: bool,
    /// Compressor's peak envelope (linear).
    comp_env: f64,
    // Limiter look-ahead: the delayed audio and the gain each sample needs.
    lim_delay: Box<[[f64; LOOKAHEAD_MAX]; MAX_CHANNELS]>,
    lim_needed: Box<[f64; LOOKAHEAD_MAX]>,
    lim_len: usize,
    lim_pos: usize,
    lim_gain: f64,
    // Noise suppression: one per audio channel, created up front (outside the audio thread).
    denoisers: Vec<Box<Denoiser>>,
    /// Delay line per audio channel (reserved up front) and where it writes next.
    delay_lines: Vec<Vec<f32>>,
    delay_pos: usize,
    delay_samples: usize,
    delay_ms: f32,
    noise_pos: usize,
    // Ducking: the sources' level envelope, hold counter and current gain (dB).
    duck_env: f64,
    duck_hold: usize,
    duck_gain_db: f64,
}

fn db(v: f64) -> f64 {
    if v <= 1e-12 { -240.0 } else { 20.0 * v.log10() }
}

/// One-pole smoothing coefficient for a time constant.
fn coef(seconds: f64, rate: f64) -> f64 {
    1.0 - (-1.0 / (seconds * rate)).exp()
}

impl Chain {
    /// `channels`: how many audio channels will be processed (noise suppression keeps
    /// state per channel, created here because the audio thread mustn't allocate).
    pub fn new(channels: usize, rate: f64, fx: &Effects) -> Self {
        let flat = EqBand::new(BandKind::Bell, 1000.0, 0.0, 1.0);
        let mut chain = Chain {
            rate,
            low_cut: false,
            low_cut_hz: 80.0,
            gate: Gate::default(),
            comp: Comp::default(),
            limiter: Limiter::default(),
            noise: NoiseSuppression::default(),
            deess: DeEsser::default(),
            duck: DuckParams::default(),
            mono: false,
            eq_enabled: false,
            bands: [flat; EQ_MAX_BANDS],
            band_count: 0,
            low_cut_filters: [Biquad::default(); MAX_CHANNELS],
            eq_filters: [[Biquad::default(); EQ_MAX_BANDS]; MAX_CHANNELS],
            deess_filters: [Biquad::default(); MAX_CHANNELS],
            deess_detect: [Biquad::default(); MAX_CHANNELS],
            deess_env: 0.0,
            gate_gain_db: 0.0,
            gate_hold: 0,
            gate_open: true,
            comp_env: 0.0,
            lim_delay: Box::new([[0.0; LOOKAHEAD_MAX]; MAX_CHANNELS]),
            lim_needed: Box::new([1.0; LOOKAHEAD_MAX]),
            lim_len: 0,
            lim_pos: 0,
            lim_gain: 1.0,
            denoisers: (0..channels.min(MAX_CHANNELS))
                .map(|_| Denoiser::new())
                .collect(),
            delay_lines: (0..channels.min(MAX_CHANNELS))
                .map(|_| vec![0.0; DELAY_ROOM])
                .collect(),
            delay_pos: 0,
            delay_samples: 0,
            delay_ms: 0.0,
            noise_pos: 0,
            duck_env: 0.0,
            duck_hold: 0,
            duck_gain_db: 0.0,
        };
        chain.set(fx);
        chain
    }

    /// New settings. Keeps the filters' running state, so changes don't click.
    /// Doesn't allocate.
    pub fn set(&mut self, fx: &Effects) {
        self.low_cut = fx.low_cut;
        self.low_cut_hz = fx.low_cut_hz;
        self.gate = fx.gate;
        self.comp = fx.comp;
        if fx.limiter.enabled && !self.limiter.enabled {
            self.reset_limiter();
        }
        self.limiter = fx.limiter;
        if fx.noise.enabled && !self.noise.enabled {
            self.noise_pos = 0;
            for d in &mut self.denoisers {
                d.clear();
            }
        }
        self.noise = fx.noise;
        self.deess = fx.deess;
        self.mono = fx.mono;
        if fx.delay_ms != self.delay_ms {
            self.delay_ms = fx.delay_ms;
            self.update_delay();
        }
        self.duck = DuckParams {
            enabled: fx.duck.enabled && !fx.duck.sources.is_empty(),
            amount_db: (fx.duck.amount_db as f64).clamp(0.0, 60.0),
            threshold_db: fx.duck.threshold_db as f64,
            attack_ms: fx.duck.attack_ms,
            hold_ms: fx.duck.hold_ms,
            release_ms: fx.duck.release_ms,
        };
        self.eq_enabled = fx.eq.enabled;
        self.band_count = fx.eq.bands.len().min(EQ_MAX_BANDS);
        self.bands[..self.band_count].copy_from_slice(&fx.eq.bands[..self.band_count]);
        self.design();
    }

    pub fn set_rate(&mut self, rate: f64) {
        if rate > 0.0 && rate != self.rate {
            self.rate = rate;
            self.design();
            self.reset_limiter();
            self.update_delay();
        }
    }

    /// The delay in samples at the current rate (capped at the reserved room).
    fn update_delay(&mut self) {
        let wanted =
            (self.delay_ms.clamp(0.0, MAX_DELAY_MS) as f64 / 1000.0 * self.rate).round() as usize;
        let new = wanted.min(DELAY_ROOM - 1);
        if new != self.delay_samples && self.delay_samples == 0 {
            // Starting fresh: no stale audio from a previous delay.
            for line in &mut self.delay_lines {
                line.fill(0.0);
            }
        }
        self.delay_samples = new;
    }

    /// Empty look-ahead (when the limiter is switched on or the sample rate changes).
    fn reset_limiter(&mut self) {
        self.lim_len = ((LIMITER_LOOKAHEAD_S * self.rate) as usize).clamp(1, LOOKAHEAD_MAX);
        self.lim_pos = 0;
        self.lim_gain = 1.0;
        for d in self.lim_delay.iter_mut() {
            d.fill(0.0);
        }
        self.lim_needed.fill(1.0);
    }

    fn design(&mut self) {
        let low_cut = Biquad::low_cut(self.low_cut_hz, self.rate);
        let mut eq = [Biquad::default(); EQ_MAX_BANDS];
        for (f, b) in eq.iter_mut().zip(&self.bands[..self.band_count]) {
            *f = Biquad::band(b, self.rate);
        }
        let deess_freq = (self.deess.freq as f64).clamp(2000.0, 16000.0);
        let split = Biquad::designed(Shape::LowPass, deess_freq, 0.0, self.rate);
        let detect = Biquad::designed(Shape::HighPass, deess_freq, 0.0, self.rate);
        for c in 0..MAX_CHANNELS {
            self.low_cut_filters[c].take_coefficients(&low_cut);
            self.deess_filters[c].take_coefficients(&split);
            self.deess_detect[c].take_coefficients(&detect);
            for (f, d) in self.eq_filters[c].iter_mut().zip(&eq[..self.band_count]) {
                f.take_coefficients(d);
            }
        }
    }

    /// Process audio in place. `bufs` holds one buffer per audio channel, all the same
    /// length. `fader_db` is the channel's fader, which the audio has already been
    /// through: gate and compressor compensate for it.
    /// `sidechain`: the ducking sources' audio (may be empty, or shorter = silence).
    pub fn process(
        &mut self,
        bufs: &mut [&mut [f32]],
        sidechain: &[&[f32]],
        fader_db: f32,
    ) -> Stats {
        let n = bufs.len().min(MAX_CHANNELS);
        let bufs = &mut bufs[..n];
        let frames = bufs.iter().map(|b| b.len()).min().unwrap_or(0);
        let fader = fader_db as f64;
        let (gate, comp) = (self.gate, self.comp);
        let ms = |v: f32, min: f64| (v as f64).max(min) / 1000.0;

        let gate_threshold = gate.threshold_db as f64;
        let gate_range = (gate.range_db as f64).clamp(6.0, 120.0);
        // dB per sample to go fully open / fully closed in the attack / release time.
        let gate_att = gate_range / (ms(gate.attack_ms, 0.05) * self.rate);
        let gate_rel = gate_range / (ms(gate.release_ms, 1.0) * self.rate);
        let hold = (ms(gate.hold_ms, 0.0) * self.rate) as usize;

        let comp_att = coef(ms(comp.attack_ms, 0.05), self.rate);
        let comp_rel = coef(ms(comp.release_ms, 1.0), self.rate);
        let ratio = (comp.ratio as f64).max(1.0);
        let comp_threshold = comp.threshold_db as f64;
        let knee = (comp.knee_db as f64).clamp(0.0, 24.0);
        let makeup = 10f64.powf(comp.makeup_db as f64 / 20.0);

        let limiter = self.limiter;
        let ceiling = 10f64.powf((limiter.ceiling_db as f64).min(0.0) / 20.0);
        let lim_len = self.lim_len.max(1);
        // Reach the needed gain within the look-ahead, so it's there when the peak is.
        let lim_att = 1.0 - (-5.0 / lim_len as f64).exp();
        let lim_rel = coef(ms(limiter.release_ms, 1.0), self.rate);
        let deess = self.deess;
        let deess_threshold = deess.threshold_db as f64;
        let deess_amount = (deess.amount_db as f64).clamp(0.0, 30.0);
        // Fast enough to catch an "s", slow enough not to distort it.
        let (deess_att, deess_rel) = (coef(0.001, self.rate), coef(0.06, self.rate));
        let mut deess_max = 0f64;
        let duck = self.duck;
        // dB per sample down / up, and how long it stays down after the sources go quiet.
        let duck_down = duck.amount_db / (ms(duck.attack_ms, 1.0) * self.rate);
        let duck_up = duck.amount_db / (ms(duck.release_ms, 1.0) * self.rate);
        let duck_hold = (ms(duck.hold_ms, 0.0) * self.rate) as usize;
        let (env_att, env_rel) = (coef(0.005, self.rate), coef(0.1, self.rate));
        let mut duck_max = 0f64;
        let mut source_max = 0f64;
        let mut min_gain = 1.0f64;

        let mut stats = Stats {
            level_db: -120.0,
            duck_db: 0.0,
            duck_source_db: -120.0,
            gate_open: self.gate_open,
            reduction_db: 0.0,
            limit_db: 0.0,
            deess_db: 0.0,
        };
        let mut highs = [0f64; MAX_CHANNELS];
        let mut storage = [0f64; MAX_CHANNELS];
        let frame = &mut storage[..n];

        for i in 0..frames {
            for (c, b) in bufs.iter().enumerate() {
                frame[c] = b[i] as f64;
            }
            if self.low_cut {
                for (x, f) in frame.iter_mut().zip(&mut self.low_cut_filters) {
                    *x = f.run(*x);
                }
            }
            if self.noise.enabled && !self.denoisers.is_empty() {
                let amount = self.noise.amount.clamp(0.0, 1.0) as f64;
                let pos = self.noise_pos;
                for (x, d) in frame.iter_mut().zip(self.denoisers.iter_mut()) {
                    // RNNoise works in 16-bit sample units.
                    d.input[pos] = (*x * 32768.0) as f32;
                    let wet = d.wet[pos] as f64 / 32768.0;
                    let dry = d.dry[pos] as f64 / 32768.0;
                    *x = wet * amount + dry * (1.0 - amount);
                }
                self.noise_pos += 1;
                if self.noise_pos == NOISE_BLOCK {
                    self.noise_pos = 0;
                    for d in self.denoisers.iter_mut() {
                        let Denoiser {
                            state,
                            input,
                            wet,
                            dry,
                        } = &mut **d;
                        state.process_frame(wet, input);
                        dry.copy_from_slice(input);
                    }
                }
            }
            if self.mono {
                // Left and right of each pair (front, rear, side) mixed together.
                for (l, r) in [(0, 1), (4, 5), (6, 7)] {
                    if r < frame.len() {
                        let m = (frame[l] + frame[r]) * 0.5;
                        frame[l] = m;
                        frame[r] = m;
                    }
                }
            }
            // Level before the fader, loudest channel.
            let peak = frame.iter().fold(0f64, |m, x| m.max(x.abs()));
            let level = db(peak) - fader;
            stats.level_db = stats.level_db.max(level as f32);

            if gate.enabled {
                if level > gate_threshold {
                    self.gate_open = true;
                    self.gate_hold = hold;
                } else if level < gate_threshold - GATE_HYSTERESIS_DB {
                    if self.gate_hold > 0 {
                        self.gate_hold -= 1;
                    } else {
                        self.gate_open = false;
                    }
                }
                self.gate_gain_db = if self.gate_open {
                    (self.gate_gain_db + gate_att).min(0.0)
                } else {
                    (self.gate_gain_db - gate_rel).max(-gate_range)
                };
                let g = 10f64.powf(self.gate_gain_db / 20.0);
                for x in frame.iter_mut() {
                    *x *= g;
                }
            } else {
                self.gate_open = true;
                self.gate_gain_db = 0.0;
            }

            if self.eq_enabled {
                for (x, filters) in frame.iter_mut().zip(&mut self.eq_filters) {
                    for f in &mut filters[..self.band_count] {
                        *x = f.run(*x);
                    }
                }
            }

            if deess.enabled {
                let mut det = 0f64;
                for (c, x) in frame.iter().enumerate() {
                    highs[c] = x - self.deess_filters[c].run(*x);
                    det = det.max(self.deess_detect[c].run(*x).abs());
                }
                let k = if det > self.deess_env {
                    deess_att
                } else {
                    deess_rel
                };
                self.deess_env += (det - self.deess_env) * k;
                // 4:1 above the threshold (judged before the fader), up to the amount.
                let over = db(self.deess_env) - fader - deess_threshold;
                let cut = (over * 0.75).clamp(0.0, deess_amount);
                deess_max = deess_max.max(cut);
                if cut > 0.0 {
                    let g = 10f64.powf(-cut / 20.0);
                    for (x, h) in frame.iter_mut().zip(&highs) {
                        *x -= h * (1.0 - g);
                    }
                }
            }

            if comp.enabled {
                // Follow the peaks (fast up, slow down), so a waveform's zero crossings
                // don't drag the reading down.
                let det = frame.iter().fold(0f64, |m, x| m.max(x.abs()));
                let k = if det > self.comp_env {
                    comp_att
                } else {
                    comp_rel
                };
                self.comp_env += (det - self.comp_env) * k;
                // Soft-knee gain computer, on the level before the fader.
                let over = db(self.comp_env) - fader - comp_threshold;
                let slope = 1.0 / ratio - 1.0;
                let gr_db = if 2.0 * over < -knee {
                    0.0
                } else if knee > 0.0 && 2.0 * over.abs() <= knee {
                    slope * (over + knee / 2.0).powi(2) / (2.0 * knee)
                } else {
                    slope * over
                };
                stats.reduction_db = stats.reduction_db.max(-gr_db as f32);
                let g = 10f64.powf(gr_db / 20.0) * makeup;
                for x in frame.iter_mut() {
                    *x *= g;
                }
            }

            if duck.enabled {
                // How loud the sources are right now.
                let src = sidechain
                    .iter()
                    .fold(0f64, |m, b| m.max(b.get(i).map_or(0.0, |v| v.abs() as f64)));
                let k = if src > self.duck_env {
                    env_att
                } else {
                    env_rel
                };
                self.duck_env += (src - self.duck_env) * k;
                source_max = source_max.max(self.duck_env);
                if db(self.duck_env) > duck.threshold_db {
                    self.duck_hold = duck_hold;
                } else if self.duck_hold > 0 {
                    self.duck_hold -= 1;
                }
                let target = if self.duck_hold > 0 {
                    -duck.amount_db
                } else {
                    0.0
                };
                self.duck_gain_db = if target < self.duck_gain_db {
                    (self.duck_gain_db - duck_down).max(target)
                } else {
                    (self.duck_gain_db + duck_up).min(target)
                };
                duck_max = duck_max.max(-self.duck_gain_db);
                let g = 10f64.powf(self.duck_gain_db / 20.0);
                for x in frame.iter_mut() {
                    *x *= g;
                }
            } else {
                self.duck_gain_db = 0.0;
                self.duck_hold = 0;
            }

            if limiter.enabled {
                let peak = frame.iter().fold(0f64, |m, x| m.max(x.abs()));
                let pos = self.lim_pos;
                self.lim_needed[pos] = if peak > ceiling { ceiling / peak } else { 1.0 };
                // Play the audio from a moment ago; keep the new sample for later.
                for (c, x) in frame.iter_mut().enumerate() {
                    std::mem::swap(x, &mut self.lim_delay[c][pos]);
                }
                self.lim_pos = (pos + 1) % lim_len;
                // The lowest gain any sample in the look-ahead needs.
                let need = self.lim_needed[..lim_len]
                    .iter()
                    .fold(1.0f64, |m, g| m.min(*g));
                let k = if need < self.lim_gain {
                    lim_att
                } else {
                    lim_rel
                };
                self.lim_gain += (need - self.lim_gain) * k;
                min_gain = min_gain.min(self.lim_gain);
                for x in frame.iter_mut() {
                    // Safety: whatever slipped through is clipped at the ceiling.
                    *x = (*x * self.lim_gain).clamp(-ceiling, ceiling);
                }
            }

            for (c, b) in bufs.iter_mut().enumerate() {
                b[i] = frame[c] as f32;
            }
            if self.delay_samples > 0 {
                // Write now, play what was written `delay_samples` ago.
                let room = DELAY_ROOM;
                let read = (self.delay_pos + room - self.delay_samples) % room;
                for (b, line) in bufs.iter_mut().zip(self.delay_lines.iter_mut()) {
                    line[self.delay_pos] = b[i];
                    b[i] = line[read];
                }
                self.delay_pos = (self.delay_pos + 1) % room;
            }
        }
        stats.limit_db = -db(min_gain) as f32;
        stats.duck_db = duck_max as f32;
        stats.deess_db = deess_max as f32;
        stats.duck_source_db = db(source_max) as f32;
        stats.gate_open = !gate.enabled || self.gate_open;
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48000.0;

    fn sine(freq: f64, amp: f64, secs: f64) -> Vec<f32> {
        (0..(RATE * secs) as usize)
            .map(|i| (amp * (2.0 * std::f64::consts::PI * freq * i as f64 / RATE).sin()) as f32)
            .collect()
    }

    /// Peak level (dB) of the last half of a buffer, after the effects settled.
    fn tail_db(b: &[f32]) -> f64 {
        db(b[b.len() / 2..]
            .iter()
            .fold(0f64, |m, x| m.max(x.abs() as f64)))
    }

    fn run(fx: &Effects, freq: f64, amp: f64, fader_db: f32) -> (f64, Stats) {
        let mut chain = Chain::new(8, RATE, fx);
        let (mut l, mut r) = (sine(freq, amp, 1.0), sine(freq, amp, 1.0));
        let stats = chain.process(&mut [&mut l, &mut r], &[], fader_db);
        (tail_db(&l), stats)
    }

    fn db_to_amp(d: f64) -> f64 {
        10f64.powf(d / 20.0)
    }

    fn with_eq(bands: Vec<EqBand>) -> Effects {
        let mut fx = Effects::default();
        fx.eq.enabled = true;
        fx.eq.bands = bands;
        fx
    }

    fn gate(threshold_db: f32) -> Effects {
        Effects {
            gate: Gate {
                enabled: true,
                threshold_db,
                ..Gate::default()
            },
            ..Default::default()
        }
    }

    fn comp(makeup_db: f32) -> Effects {
        Effects {
            comp: Comp {
                enabled: true,
                threshold_db: -20.0,
                ratio: 4.0,
                makeup_db,
                ..Comp::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn nothing_on_changes_nothing() {
        let (out, _) = run(&Effects::default(), 1000.0, 0.5, 0.0);
        assert!((out - db(0.5)).abs() < 0.01);
    }

    #[test]
    fn eq_points_hit_their_frequencies() {
        let base = db(0.25);
        let change = |fx: &Effects, f| run(fx, f, 0.25, 0.0).0 - base;
        let bell = with_eq(vec![EqBand::new(BandKind::Bell, 1000.0, 6.0, 1.0)]);
        assert!(
            (change(&bell, 1000.0) - 6.0).abs() < 0.3,
            "bell +6 at 1 kHz"
        );
        assert!(change(&bell, 60.0).abs() < 0.5, "bell leaves bass alone");
        let low = with_eq(vec![EqBand::new(BandKind::LowShelf, 100.0, -9.0, 0.7)]);
        assert!(
            (change(&low, 30.0) + 9.0).abs() < 0.7,
            "bass shelf -9 well below 100 Hz"
        );
        let high = with_eq(vec![EqBand::new(BandKind::HighShelf, 8000.0, 6.0, 0.7)]);
        let treble = change(&high, 20000.0);
        assert!(
            (4.5..=6.3).contains(&treble),
            "treble shelf high up: {treble}"
        );
        assert!(
            change(&high, 300.0).abs() < 0.3,
            "treble shelf leaves the low mids alone"
        );
        // Several points add up; an empty point list is flat.
        let two = with_eq(vec![
            EqBand::new(BandKind::Bell, 1000.0, 3.0, 1.0),
            EqBand::new(BandKind::Bell, 1000.0, 3.0, 1.0),
        ]);
        assert!((change(&two, 1000.0) - 6.0).abs() < 0.3);
        assert!(change(&with_eq(vec![]), 1000.0).abs() < 0.01);
    }

    #[test]
    fn many_points_up_to_the_maximum() {
        let bands = (0..EQ_MAX_BANDS)
            .map(|i| EqBand::at(50.0 * 1.5f32.powi(i as i32), 1.0))
            .collect();
        let fx = with_eq(bands);
        let (out, _) = run(&fx, 1000.0, 0.1, 0.0);
        let drawn = response_db(&fx, 1000.0);
        assert!((out - db(0.1) - drawn).abs() < 0.3);
    }

    #[test]
    fn drawn_curve_matches_what_is_heard() {
        let mut fx = with_eq(vec![
            EqBand::new(BandKind::LowShelf, 100.0, -4.0, 0.7),
            EqBand::new(BandKind::Bell, 1000.0, 6.0, 1.0),
            EqBand::new(BandKind::Bell, 4000.0, -3.0, 2.0),
        ]);
        fx.low_cut = true;
        fx.low_cut_hz = 120.0;
        for freq in [40.0, 200.0, 1000.0, 4000.0, 9000.0] {
            let heard = run(&fx, freq, 0.25, 0.0).0 - db(0.25);
            let drawn = response_db(&fx, freq);
            assert!(
                (heard - drawn).abs() < 0.3,
                "{freq} Hz: heard {heard:.2}, drawn {drawn:.2}"
            );
        }
    }

    #[test]
    fn changing_points_keeps_running() {
        // Settings can change between blocks (as from the GUI) without a reset.
        let mut chain = Chain::new(
            8,
            RATE,
            &with_eq(vec![EqBand::new(BandKind::Bell, 1000.0, 6.0, 1.0)]),
        );
        let mut a = sine(1000.0, 0.25, 0.5);
        chain.process(&mut [&mut a], &[], 0.0);
        chain.set(&with_eq(vec![]));
        let mut b = sine(1000.0, 0.25, 0.5);
        chain.process(&mut [&mut b], &[], 0.0);
        assert!(
            (tail_db(&b) - db(0.25)).abs() < 0.1,
            "flat after removing the point"
        );
    }

    #[test]
    fn low_cut_removes_rumble_only() {
        let fx = Effects {
            low_cut: true,
            ..Default::default()
        };
        assert!(
            run(&fx, 30.0, 0.5, 0.0).0 - db(0.5) < -12.0,
            "30 Hz rumble cut"
        );
        assert!(
            (run(&fx, 1000.0, 0.5, 0.0).0 - db(0.5)).abs() < 0.1,
            "voice untouched"
        );
    }

    #[test]
    fn gate_closes_on_quiet_and_opens_on_loud() {
        let fx = gate(-40.0);
        let (quiet, s) = run(&fx, 1000.0, 0.001, 0.0); // -60 dB background noise
        assert!(quiet < -60.0 - 70.0, "noise silenced: {quiet}");
        assert!(!s.gate_open);
        let (loud, s) = run(&fx, 1000.0, 0.1, 0.0); // -20 dB speech
        assert!((loud - db(0.1)).abs() < 0.1 && s.gate_open);
    }

    #[test]
    fn gate_judges_the_level_before_the_fader() {
        // -30 dB voice, fader at -20: after the fader it's -50 dB, but the gate must
        // still see -30 and stay open.
        let (_, s) = run(&gate(-40.0), 1000.0, db_to_amp(-50.0), -20.0);
        assert!(s.gate_open);
    }

    #[test]
    fn gate_timing_follows_its_settings() {
        // A long hold keeps the gate open after the sound stops.
        let mut fx = gate(-40.0);
        fx.gate.hold_ms = 400.0;
        let mut chain = Chain::new(8, RATE, &fx);
        let mut loud = sine(1000.0, 0.1, 0.2);
        chain.process(&mut [&mut loud], &[], 0.0);
        let mut quiet = sine(1000.0, 0.001, 0.3); // within the hold time
        assert!(chain.process(&mut [&mut quiet], &[], 0.0).gate_open);
        // A small range only turns the noise down a little.
        let mut fx = gate(-40.0);
        fx.gate.range_db = 10.0;
        let (quiet, _) = run(&fx, 1000.0, 0.001, 0.0);
        assert!((quiet - (-60.0 - 10.0)).abs() < 0.5, "{quiet}");
    }

    #[test]
    fn compressor_turns_down_by_the_ratio() {
        // 0 dB peaks, 20 dB over the threshold: at 4:1 that leaves 5 dB over, so 15 dB less.
        let (out, s) = run(&comp(0.0), 1000.0, 1.0, 0.0);
        assert!((out + 15.0).abs() < 1.0, "out {out}");
        assert!((s.reduction_db - 15.0).abs() < 1.0);
        // Below the threshold (and its soft knee) nothing happens.
        let (quiet, s) = run(&comp(0.0), 1000.0, db_to_amp(-40.0), 0.0);
        assert!((quiet + 40.0).abs() < 0.1 && s.reduction_db < 0.1);
        // Makeup gain is added on top.
        assert!((run(&comp(6.0), 1000.0, 1.0, 0.0).0 - (out + 6.0)).abs() < 0.2);
    }

    #[test]
    fn compressor_judges_the_level_before_the_fader() {
        // 0 dB source, fader at -10: still 15 dB of reduction, as with the fader at 0.
        let (_, s) = run(&comp(0.0), 1000.0, db_to_amp(-10.0), -10.0);
        assert!((s.reduction_db - 15.0).abs() < 1.0, "{}", s.reduction_db);
    }

    fn limiter(ceiling_db: f32) -> Effects {
        Effects {
            limiter: Limiter {
                enabled: true,
                ceiling_db,
                ..Limiter::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn limiter_holds_the_ceiling() {
        // Loud 0 dB sine, ceiling -6 dB: comes out at the ceiling, never above.
        let (out, s) = run(&limiter(-6.0), 1000.0, 1.0, 0.0);
        assert!(out <= -6.0 + 0.01 && out > -6.5, "out {out}");
        assert!((s.limit_db - 6.0).abs() < 0.5, "{}", s.limit_db);
        // Quiet sound passes untouched (just a little later).
        let (quiet, s) = run(&limiter(-1.0), 1000.0, 0.1, 0.0);
        assert!((quiet - db(0.1)).abs() < 0.01 && s.limit_db < 0.01);
    }

    #[test]
    fn limiter_catches_a_sudden_peak_without_overshoot() {
        let mut chain = Chain::new(8, RATE, &limiter(-3.0));
        // Silence, then a sudden full-scale burst.
        let mut buf: Vec<f32> = vec![0.0; 4800];
        buf.extend(sine(1000.0, 1.0, 0.2));
        chain.process(&mut [&mut buf], &[], 0.0);
        let ceiling = db_to_amp(-3.0) as f32;
        assert!(
            buf.iter().all(|x| x.abs() <= ceiling + 1e-6),
            "nothing above the ceiling"
        );
        // And the look-ahead turned it down smoothly instead of clipping: from the very
        // first wave, every sample is the input times one steady gain (clipping would
        // flatten the tops and change that ratio).
        let source = sine(1000.0, 1.0, 0.2);
        let start = 4800 + (LIMITER_LOOKAHEAD_S * RATE) as usize;
        for k in 0..96 {
            if source[k].abs() > 0.3 {
                let ratio = buf[start + k] / source[k];
                assert!((0.69..=0.72).contains(&ratio), "sample {k}: ratio {ratio}");
            }
        }
    }

    fn noise(amount: f32) -> Effects {
        Effects {
            noise: NoiseSuppression {
                enabled: true,
                amount,
            },
            ..Default::default()
        }
    }

    fn white_noise(amp: f32, secs: f64) -> Vec<f32> {
        let mut seed = 0x9e37_79b9u32;
        (0..(RATE * secs) as usize)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    fn rms_db(b: &[f32]) -> f64 {
        db((b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / b.len() as f64).sqrt())
    }

    #[test]
    fn noise_suppression_turns_hiss_down() {
        // RNNoise is trained on real-world noise and speech; on pure white hiss its result
        // depends a lot on where its 10 ms blocks fall (measured: 1 to 28 dB). So check
        // several alignments: never louder, and clearly quieter on average.
        let hiss = white_noise(0.03, 4.0);
        let input = rms_db(&hiss[3 * RATE as usize..]);
        let mut total = 0.0;
        let offsets = [0usize, 47, 100, 240];
        for offset in offsets {
            let mut chain = Chain::new(1, RATE, &noise(1.0));
            let mut buf: Vec<f32> = std::iter::repeat_n(0.0, offset)
                .chain(hiss.iter().copied())
                .collect();
            chain.process(&mut [&mut buf], &[], 0.0);
            let out = rms_db(&buf[buf.len() - RATE as usize..]);
            assert!(out < input, "offset {offset}: {input:.1} -> {out:.1} dB");
            total += input - out;
        }
        let average = total / offsets.len() as f64;
        assert!(average > 3.0, "on average {average:.1} dB quieter");
    }

    #[test]
    fn noise_suppression_delay_and_mix_line_up() {
        // With amount 0 it's the original, exactly one block later (so mixing lines up).
        let mut chain = Chain::new(1, RATE, &noise(0.0));
        let source = white_noise(0.2, 0.2);
        let mut buf = source.clone();
        chain.process(&mut [&mut buf], &[], 0.0);
        for i in NOISE_BLOCK..source.len() {
            assert!(
                (buf[i] - source[i - NOISE_BLOCK]).abs() < 1e-4,
                "sample {i}"
            );
        }
    }

    fn ducking(amount_db: f32) -> Effects {
        let mut fx = Effects::default();
        fx.duck = crate::config::Duck {
            enabled: true,
            sources: vec!["mic".into()],
            amount_db,
            ..Default::default()
        };
        fx
    }

    #[test]
    fn ducks_while_the_source_is_active() {
        let mut chain = Chain::new(2, RATE, &ducking(12.0));
        let (mut l, mut r) = (sine(500.0, 0.25, 1.5), sine(500.0, 0.25, 1.5));
        let voice = sine(200.0, 0.1, 1.5); // -20 dB, above the -40 dB threshold
        let s = chain.process(&mut [&mut l, &mut r], &[&voice], 0.0);
        assert!(
            (tail_db(&l) - (db(0.25) - 12.0)).abs() < 0.2,
            "turned down 12 dB: {}",
            tail_db(&l)
        );
        assert!((s.duck_db - 12.0).abs() < 0.1);
        assert!((s.duck_source_db - db(0.1) as f32).abs() < 1.0);
    }

    #[test]
    fn comes_back_after_hold_and_release() {
        let fx = ducking(12.0);
        let mut chain = Chain::new(1, RATE, &fx);
        let mut music = sine(500.0, 0.25, 0.5);
        chain.process(&mut [&mut music], &[&sine(200.0, 0.1, 0.5)], 0.0);
        // Source goes quiet: still down during the hold...
        let mut music = sine(500.0, 0.25, 0.3);
        chain.process(&mut [&mut music], &[], 0.0);
        assert!(tail_db(&music) < db(0.25) - 11.0, "held down");
        // ...and fully back after the source's level has faded, the hold and the release
        // (about 0.25 + 0.4 + 0.6 s): the next stretch is untouched.
        let mut music = sine(500.0, 0.25, 1.5);
        chain.process(&mut [&mut music], &[], 0.0);
        let mut later = sine(500.0, 0.25, 0.5);
        let s = chain.process(&mut [&mut later], &[], 0.0);
        assert!(
            (tail_db(&later) - db(0.25)).abs() < 0.01 && s.duck_db == 0.0,
            "{} {}",
            tail_db(&later),
            s.duck_db
        );
    }

    #[test]
    fn quiet_source_does_not_duck() {
        let mut chain = Chain::new(1, RATE, &ducking(12.0));
        let mut music = sine(500.0, 0.25, 1.0);
        let hum = sine(50.0, 0.001, 1.0); // -60 dB, under the threshold
        let s = chain.process(&mut [&mut music], &[&hum], 0.0);
        assert!((tail_db(&music) - db(0.25)).abs() < 0.01 && s.duck_db == 0.0);
    }

    #[test]
    fn mono_mixes_left_and_right() {
        let fx = Effects {
            mono: true,
            ..Default::default()
        };
        let mut chain = Chain::new(2, RATE, &fx);
        let mut l = sine(500.0, 0.5, 0.2);
        let mut r = vec![0.0; l.len()]; // sound on the left only
        chain.process(&mut [&mut l, &mut r], &[], 0.0);
        assert!((tail_db(&l) - db(0.25)).abs() < 0.01 && (tail_db(&r) - db(0.25)).abs() < 0.01);
    }

    #[test]
    fn delay_holds_the_sound_back_exactly() {
        let fx = Effects {
            delay_ms: 250.0,
            ..Default::default()
        };
        let mut chain = Chain::new(2, RATE, &fx);
        let mut l = vec![0.0f32; 24000];
        let mut r = vec![0.0f32; 24000];
        l[100] = 1.0; // a click
        r[100] = 0.5;
        chain.process(&mut [&mut l, &mut r], &[], 0.0);
        let at = 100 + 12000; // 250 ms at 48 kHz
        assert_eq!((l[at], r[at]), (1.0, 0.5));
        assert_eq!(l.iter().filter(|x| **x != 0.0).count(), 1, "nothing else");
        // No delay: untouched.
        let mut chain = Chain::new(1, RATE, &Effects::default());
        let mut b = vec![0.0f32; 1000];
        b[10] = 1.0;
        chain.process(&mut [&mut b], &[], 0.0);
        assert_eq!(b[10], 1.0);
    }

    #[test]
    fn surround_channels_are_processed_together() {
        let mut chain = Chain::new(8, RATE, &gate(-40.0));
        let mut bufs: Vec<Vec<f32>> = (0..8).map(|_| vec![0.0; 48000]).collect();
        bufs[2] = sine(1000.0, 0.1, 1.0); // only the centre speaks
        let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| b.as_mut_slice()).collect();
        let s = chain.process(&mut refs, &[], 0.0);
        assert!(
            s.gate_open,
            "loud centre keeps the gate open for all channels"
        );
        assert!((tail_db(&bufs[2]) - db(0.1)).abs() < 0.1);
    }

    fn deesser() -> Effects {
        let mut fx = Effects::default();
        fx.deess.enabled = true; // 5.5 kHz, -30 dB, up to 10 dB
        fx
    }

    #[test]
    fn deesser_turns_down_loud_sharp_sounds_only() {
        let fx = deesser();
        // A loud "s" (8 kHz at -10 dB): 20 dB over, 4:1 -> 15, capped at 10 dB.
        let (out, stats) = run(&fx, 8000.0, db_to_amp(-10.0), 0.0);
        assert!((-23.0..-18.0).contains(&out), "sharp sound now {out:.1} dB");
        assert!(stats.deess_db > 9.0, "reported {}", stats.deess_db);
        // A loud low voice (300 Hz at -6 dB) is left alone.
        let (out, stats) = run(&fx, 300.0, db_to_amp(-6.0), 0.0);
        assert!((out + 6.0).abs() < 0.2, "voice now {out:.2} dB");
        assert!(stats.deess_db < 0.5);
        // The fader doesn't change when it acts: same "s" with the fader at -20 dB
        // (the audio arrives 20 dB quieter) is turned down just as much.
        let (out, _) = run(&fx, 8000.0, db_to_amp(-30.0), -20.0);
        assert!((-43.0..-38.0).contains(&out), "after fader {out:.1} dB");
    }

    #[test]
    fn deesser_below_threshold_is_untouched() {
        let fx = deesser();
        let mut chain = Chain::new(2, RATE, &fx);
        let orig = sine(8000.0, db_to_amp(-50.0), 0.5);
        let mut l = orig.clone();
        chain.process(&mut [&mut l], &[], 0.0);
        assert_eq!(l, orig);
    }
}
