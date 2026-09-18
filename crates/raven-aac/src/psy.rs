//! What the ear will not hear: masking thresholds, transients, mid/side.
//!
//! An AAC encoder's quality is decided here, not in the bitstream. Every
//! band of the spectrum is quantised, and quantising adds noise; the art is
//! putting that noise where the signal hides it. A loud band masks quieter
//! sound in itself and, less, in its neighbours — more upwards in frequency
//! than downwards — and a noise-like band masks far more than a pure tone
//! does. Below the threshold of hearing nothing needs coding at all.
//!
//! This is a deliberately plain model, the kind the standard's informative
//! annex describes: band energies, spread across the Bark scale by a
//! two-slope function, lowered by a signal-to-mask ratio that runs from 6 dB
//! for noise to 15-40 dB for a tone (judged by the band's spectral flatness),
//! floored at the threshold of hearing. The result, a noise budget per band,
//! drives the quantiser (`quant.rs`); the rate loop in `lib.rs` then scales
//! every budget by the same factor until the frame fits its bits, so the
//! noise keeps the shape of the mask whatever the bitrate.
//!
//! The transient detector is here too. A long block smears quantisation
//! noise over 43 ms, and ahead of a sudden attack — a consonant, a click, a
//! drum — that noise is heard as a pre-echo. Where the high-passed energy of
//! a 128-sample segment jumps well above what came before, the frame is
//! coded as eight short blocks instead.

use crate::Layout;

/// Signal-to-mask ratio, in dB, for a band that is pure noise…
const SMR_NOISE_DB: f32 = 6.0;
/// …and for a pure tone, this plus the band's Bark: noise hides far less
/// well under a tone, and less well the higher the tone (the tone-masking-
/// noise index of the standard's psychoacoustic model 1).
const SMR_TONE_DB: f32 = 14.5;
/// Masking falls off at most this fast towards higher frequencies, per
/// Bark, less the louder the masker (a fifth of a dB per dB SPL)…
const SPREAD_UP_DB: f32 = 24.0;
/// …but never slower than this…
const SPREAD_UP_MIN_DB: f32 = 8.0;
/// …and this fast towards lower ones.
const SPREAD_DOWN_DB: f32 = 25.0;
/// The threshold of hearing never asks for more than this (dB SPL): playback
/// levels are unknown, and the curve climbs steeply at the top of the band.
const ATH_CAP_DB: f32 = 40.0;
/// Full-scale 16-bit input is taken to play at 96 dB SPL.
const FULL_SCALE_SPL: f32 = 96.0;
/// The energy, in dB, a full-scale sine puts into the lines of a long block
/// at the standard's MDCT scale (`2 × 16-bit × Σ window / 2`, squared).
const FULL_SCALE_LONG_DB: f32 = 150.0;
/// A short block is an eighth as long, so its lines are 18 dB lower.
const FULL_SCALE_SHORT_DB: f32 = FULL_SCALE_LONG_DB - 18.06;

/// Per-band constants of the model, for one of the two transform lengths.
#[derive(Debug, Clone)]
pub(crate) struct BandModel {
    /// The threshold of hearing in each band, as energy per window.
    ath: Vec<f32>,
    /// Each band's centre, in Bark.
    centre: Vec<f32>,
    /// Add to a band's energy in dB to get its level in dB SPL.
    to_spl: f32,
}

/// Zwicker's critical band rate for a frequency.
fn bark(hz: f32) -> f32 {
    13.0 * (0.000_76 * hz).atan() + 3.5 * (hz / 7500.0).powi(2).atan()
}

/// Terhardt's approximation of the threshold of hearing, dB SPL.
fn hearing_threshold(hz: f32) -> f32 {
    let khz = (hz / 1000.0).max(0.02);
    3.64 * khz.powf(-0.8) - 6.5 * (-0.6 * (khz - 3.3).powi(2)).exp() + 1e-3 * khz.powi(4)
}

impl BandModel {
    /// The model for bands `edges` of a transform of `lines` lines.
    pub(crate) fn new(edges: &[u16], lines: usize, sample_rate: u32) -> Self {
        let hz = |line: f32| line * sample_rate as f32 / (2 * lines) as f32;
        let full_scale = if lines == 1024 {
            FULL_SCALE_LONG_DB
        } else {
            FULL_SCALE_SHORT_DB
        };
        let bands = edges.len() - 1;
        let mut centre = Vec::with_capacity(bands);
        let mut ath = Vec::with_capacity(bands);
        for pair in edges.windows(2) {
            let (lo, hi) = (f32::from(pair[0]), f32::from(pair[1]));
            centre.push(bark(hz((lo + hi) / 2.0)));
            let quietest = (pair[0]..pair[1])
                .map(|k| hearing_threshold(hz(f32::from(k) + 0.5)))
                .fold(f32::INFINITY, f32::min)
                .min(ATH_CAP_DB);
            ath.push(10f32.powf((quietest - FULL_SCALE_SPL + full_scale) / 10.0));
        }
        Self {
            ath,
            centre,
            to_spl: FULL_SCALE_SPL - full_scale,
        }
    }
}

/// One channel's analysis of one frame, and then its quantisation plan.
///
/// Band-indexed vectors are indexed by the flat band `g * swb + sfb`.
#[derive(Debug, Clone)]
pub(crate) struct Channel {
    /// The frame's 1024 lines in coding order (see [`Layout`]).
    pub(crate) spec: Vec<f32>,
    /// `|spec|^¾`, what the quantiser rounds.
    pub(crate) pow: Vec<f32>,
    pub(crate) energy: Vec<f32>,
    /// The noise each band can hide, as energy.
    pub(crate) threshold: Vec<f32>,
    /// The scalefactor that puts the band's noise at its threshold.
    pub(crate) base: Vec<f32>,
    /// Offsets at or above this code the band as silence: its whole energy
    /// would be under the (scaled) threshold.
    pub(crate) zero_from: Vec<f32>,
    /// The threshold of hearing, as energy.
    hearing: Vec<f32>,
    /// The smallest scalefactor worth using: finer steps than put the
    /// band's noise at the threshold of hearing buy nothing, and neither
    /// does a step so fine that a value would pass 8191.
    pub(crate) sf_min: Vec<i32>,
    pub(crate) peak_pow: Vec<f32>,
    /// Scratch for the model, per band of one group.
    spread: Vec<f32>,
    level: Vec<f32>,
    smr: Vec<f32>,
    /// The plan: quantised lines, and each band's scalefactor and book.
    pub(crate) q: Vec<i16>,
    pub(crate) sf: Vec<i32>,
    pub(crate) book: Vec<u8>,
    pub(crate) nonzero: Vec<bool>,
    pub(crate) global_gain: i32,
    /// This channel's highest band carrying anything, plus one.
    pub(crate) max_sfb: usize,
}

/// Enough for eight groups of the widest short-block table.
pub(crate) const MAX_BANDS: usize = 8 * 16;

impl Channel {
    pub(crate) fn new() -> Self {
        Self {
            spec: vec![0.0; 1024],
            pow: vec![0.0; 1024],
            energy: vec![0.0; MAX_BANDS],
            threshold: vec![0.0; MAX_BANDS],
            hearing: vec![0.0; MAX_BANDS],
            base: vec![0.0; MAX_BANDS],
            zero_from: vec![0.0; MAX_BANDS],
            sf_min: vec![0; MAX_BANDS],
            peak_pow: vec![0.0; MAX_BANDS],
            spread: vec![0.0; 64],
            level: vec![0.0; 64],
            smr: vec![0.0; 64],
            q: vec![0; 1024],
            sf: vec![0; MAX_BANDS],
            book: vec![0; MAX_BANDS],
            nonzero: vec![false; MAX_BANDS],
            global_gain: 100,
            max_sfb: 0,
        }
    }

    /// Band energies and masking thresholds, from `spec`.
    pub(crate) fn mask(&mut self, layout: &Layout, model: &BandModel) {
        for g in 0..layout.groups {
            let windows = f32::from(layout.group_len[g]);
            let first = g * layout.swb;
            for sfb in 0..layout.swb {
                let b = first + sfb;
                let lines = &self.spec[layout.band(b)];
                let energy: f32 = lines.iter().map(|x| x * x).sum();
                self.energy[b] = energy;
                // Tonality from spectral flatness — the geometric over the
                // arithmetic mean of the power, near -5 dB for noise and far
                // below for a tone — taken over at least 16 lines around the
                // band: in a band of four, a tone's main lobe fills it and
                // looks flat. Short blocks are too coarse in frequency to
                // tell, and are treated as noise.
                let tonality = if layout.short || energy <= 0.0 {
                    0.0
                } else {
                    let range = layout.band(b);
                    let wide = 16.max(range.len());
                    let centre = (range.start + range.end) / 2;
                    let lo = centre.saturating_sub(wide / 2).min(1024 - wide);
                    let lines = &self.spec[lo..lo + wide];
                    let n = wide as f32;
                    let mean = lines.iter().map(|x| x * x).sum::<f32>() / n;
                    let floor = mean * 1e-6 + 1e-9;
                    let log_mean: f32 = lines.iter().map(|x| (x * x + floor).ln()).sum::<f32>() / n;
                    let flatness_db = 10.0 / std::f32::consts::LN_10 * (log_mean - mean.ln());
                    ((-flatness_db - 6.0) / 24.0).clamp(0.0, 1.0)
                };
                let tone = SMR_TONE_DB + model.centre[sfb];
                let smr = SMR_NOISE_DB + (tone - SMR_NOISE_DB) * tonality;
                self.smr[sfb] = 10f32.powf(-smr / 10.0);
            }
            // Spread the energies, masker by masker: upwards by a slope
            // that flattens as the masker grows louder (Terhardt's),
            // downwards by a fixed one. In dB, so a band costs a logarithm.
            let n = layout.swb;
            for (db, &e) in self.level[..n]
                .iter_mut()
                .zip(&self.energy[first..first + n])
            {
                *db = 10.0 * (e / windows).max(1e-3).log10();
            }
            for sfb in 0..n {
                let z = model.centre[sfb];
                let mut most = self.level[sfb];
                for j in 0..n {
                    let dz = z - model.centre[j];
                    let spread = if dz > 0.0 {
                        let spl = self.level[j] + model.to_spl;
                        let slope =
                            (SPREAD_UP_DB - 0.2 * spl).clamp(SPREAD_UP_MIN_DB, SPREAD_UP_DB);
                        self.level[j] - slope * dz
                    } else {
                        self.level[j] + SPREAD_DOWN_DB * dz
                    };
                    most = most.max(spread);
                }
                self.spread[sfb] = 10f32.powf(most / 10.0) * windows;
            }
            let spread = &self.spread[..n];
            let t = &mut self.threshold[first..first + layout.swb];
            for ((t, s), smr) in t.iter_mut().zip(spread).zip(&self.smr) {
                *t = s * smr;
            }
            for (sfb, t) in t.iter_mut().enumerate() {
                let ath = model.ath[sfb] * windows;
                self.hearing[first + sfb] = ath;
                *t = t.max(ath);
            }
        }
    }

    /// Everything the rate loop needs, once the spectrum is final.
    pub(crate) fn prepare(&mut self, layout: &Layout) {
        for b in 0..layout.bands() {
            let range = layout.band(b);
            let mut roots = 0.0f32;
            let mut peak = 0.0f32;
            let mut energy = 0.0f32;
            for (x, p) in self.spec[range.clone()]
                .iter()
                .zip(&mut self.pow[range.clone()])
            {
                let a = x.abs();
                let root = a.sqrt();
                *p = root * root.sqrt();
                roots += root;
                peak = peak.max(*p);
                energy += a * a;
            }
            self.energy[b] = energy;
            self.peak_pow[b] = peak;
            let t = self.threshold[b].max(1e-9);
            if roots <= 0.0 || energy <= 0.0 {
                self.base[b] = 0.0;
                self.zero_from[b] = f32::NEG_INFINITY;
                self.sf_min[b] = 0;
                continue;
            }
            // Quantising |x|^¾ with step 2^(3s/16) leaves noise of about
            // 4/27 · 2^(3s/8) · Σ√|x| in the band (the rounding's 1/12,
            // stretched by the ⁴⁄₃ power's slope); solve for the s that
            // makes that the threshold.
            let sf_for = |noise: f32| 8.0 / 3.0 * (27.0 * noise / (4.0 * roots)).log2() + 100.0;
            self.base[b] = sf_for(t);
            // A band under the threshold of hearing is never coded, however
            // many bits there are to spare.
            let hearing = self.hearing[b];
            self.zero_from[b] = if energy > hearing {
                8.0 / 3.0 * (energy / t).log2()
            } else {
                f32::NEG_INFINITY
            };
            let fits = (16.0 / 3.0 * (peak / 8191.5).log2()).floor() as i32 + 101;
            self.sf_min[b] = fits.max(sf_for(hearing).floor() as i32).max(0);
        }
    }

    /// Perceptual entropy: roughly the bits the frame needs to be
    /// transparent. It steers how much of the reservoir a frame may take.
    pub(crate) fn entropy(&self, layout: &Layout) -> f32 {
        (0..layout.bands())
            .filter(|&b| layout.sfb(b) < layout.coded)
            .map(|b| pe(self.energy[b], self.threshold[b], layout.band(b).len()))
            .sum()
    }
}

fn pe(energy: f32, threshold: f32, lines: usize) -> f32 {
    if energy > threshold && threshold > 0.0 {
        lines as f32 * (energy / threshold).log2()
    } else {
        0.0
    }
}

/// Choose mid/side per band, for a channel pair sharing `layout`, and
/// transform the bands chosen. Returns the choice per flat band.
///
/// Mid and side (`(L+R)/2`, `(L-R)/2`) win where the channels are alike:
/// the side channel is then nearly empty. The choice is by perceptual
/// entropy, with both halves held to half the stricter of the two
/// thresholds, so that the noise which lands back in left and right stays
/// masked.
pub(crate) fn mid_side(pair: &mut [Channel], layout: &Layout, ms: &mut [bool]) {
    let (left, right) = pair.split_at_mut(1);
    let (l, r) = (&mut left[0], &mut right[0]);
    for (b, ms) in ms.iter_mut().enumerate().take(layout.bands()) {
        *ms = false;
        if layout.sfb(b) >= layout.coded {
            continue;
        }
        let range = layout.band(b);
        let (mut mid, mut side) = (0.0f32, 0.0f32);
        for (a, c) in l.spec[range.clone()].iter().zip(&r.spec[range.clone()]) {
            mid += (a + c) * (a + c);
            side += (a - c) * (a - c);
        }
        let (mid, side) = (mid / 4.0, side / 4.0);
        // Left is mid plus side, so its noise is both theirs: each gets half
        // of the stricter threshold.
        let t = 0.5 * l.threshold[b].min(r.threshold[b]);
        let n = range.len();
        let separate = pe(l.energy[b], l.threshold[b], n) + pe(r.energy[b], r.threshold[b], n);
        let joint = pe(mid, t, n) + pe(side, t, n);
        if joint < separate {
            *ms = true;
            for (a, c) in l.spec[range.clone()]
                .iter_mut()
                .zip(&mut r.spec[range.clone()])
            {
                let (m, s) = ((*a + *c) * 0.5, (*a - *c) * 0.5);
                *a = m;
                *c = s;
            }
            l.threshold[b] = t;
            r.threshold[b] = t;
            l.energy[b] = mid;
            r.energy[b] = side;
        }
    }
}

/// How much louder a segment's high frequencies must be than the recent
/// past's to count as an attack.
const ATTACK_RATIO: f32 = 10.0;
/// Below this energy (a 128-sample segment's high-passed sum of squares, in
/// 16-bit units: about -60 dBFS) nothing is loud enough to pre-echo.
const ATTACK_FLOOR: f32 = 128.0 * 32.0 * 32.0;
/// How fast the memory of a loud segment fades, per segment (2.7 ms at 48
/// kHz): a click is still remembered for tens of milliseconds, so its own
/// ringing is not taken for another attack.
const ATTACK_DECAY: f32 = 0.7;

/// The transient detector for one channel.
#[derive(Debug, Clone, Default)]
pub(crate) struct Detector {
    recent: f32,
}

impl Detector {
    /// Look at `region` (preceded in time by `before`), in segments of 128.
    /// Returns whether any segment is an attack.
    pub(crate) fn attack(&mut self, before: f32, region: &[f32]) -> bool {
        let mut prev = before;
        let mut found = false;
        for segment in region.chunks_exact(128) {
            let mut energy = 0.0f32;
            for &x in segment {
                let d = x - prev;
                energy += d * d;
                prev = x;
            }
            if energy > ATTACK_FLOOR && energy > ATTACK_RATIO * self.recent {
                found = true;
            }
            self.recent = energy.max(self.recent * ATTACK_DECAY);
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_click_after_silence_is_an_attack_and_a_steady_tone_is_not() {
        let mut d = Detector::default();
        let tone: Vec<f32> = (0..4096)
            .map(|n| 10_000.0 * (n as f32 * 0.05).sin())
            .collect();
        assert!(d.attack(0.0, &tone[..1024])); // the tone starting is one
        assert!(!d.attack(tone[1023], &tone[1024..2048]));
        assert!(!d.attack(tone[2047], &tone[2048..3072]));

        let mut d = Detector::default();
        let mut click = vec![0.0f32; 1024];
        assert!(!d.attack(0.0, &click));
        click[700] = 20_000.0;
        assert!(d.attack(0.0, &click));
    }

    #[test]
    fn the_threshold_of_hearing_dips_where_the_ear_is_keenest() {
        assert!(hearing_threshold(3300.0) < 0.0);
        assert!(hearing_threshold(100.0) > 20.0);
        assert!(hearing_threshold(16_000.0) > 60.0);
    }
}
