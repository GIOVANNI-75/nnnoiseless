use crate::{
    Complex, CEPS_MEM, FRAME_SIZE, FREQ_SIZE, NB_BANDS, NB_DELTA_CEPS, NB_FEATURES, PITCH_BUF_SIZE,
    PITCH_FRAME_SIZE, PITCH_MAX_PERIOD, PITCH_MIN_PERIOD, WINDOW_SIZE,
};

/// This is the main entry-point into `nnnoiseless`. It mainly contains the various memory buffers
/// that are used while denoising. As such, this is quite a large struct, and should probably be
/// kept behind some kind of pointer.
///
/// # Example
///
/// ```rust
/// # use nnnoiseless::DenoiseState;
/// // One second of 440Hz sine wave at 48kHz sample rate. Note that the input data consists of
/// // `f32`s, but the values should be in the range of an `i16`.
/// let sine: Vec<_> = (0..48_000)
///     .map(|x| (x as f32 * 440.0 * 2.0 * std::f32::consts::PI / 48_000.0).sin() * i16::MAX as f32)
///     .collect();
/// let mut output = Vec::new();
/// let mut out_buf = [0.0; DenoiseState::FRAME_SIZE];
/// let mut denoise = DenoiseState::new();
/// let mut first = true;
/// for chunk in sine.chunks_exact(DenoiseState::FRAME_SIZE) {
///     denoise.process_frame(&mut out_buf[..], chunk);
///
///     // We throw away the first output, as discussed in the documentation for
///     //`DenoiseState::process_frame`.
///     if !first {
///         output.extend_from_slice(&out_buf[..]);
///     }
///     first = false;
/// }
/// ```
pub struct DenoiseState {
    analysis_mem: [f32; FRAME_SIZE],
    /// This is some sort of ring buffer, storing the last bunch of cepstra.
    cepstral_mem: [[f32; crate::NB_BANDS]; crate::CEPS_MEM],
    /// The index pointing to the most recent cepstrum in `cepstral_mem`. The previous cepstra are
    /// at indices mem_id - 1, mem_id - 1, etc (wrapped appropriately).
    mem_id: usize,
    synthesis_mem: [f32; FRAME_SIZE],
    pitch_buf: [f32; crate::PITCH_BUF_SIZE],
    last_gain: f32,
    last_period: usize,
    mem_hp_x: [f32; 2],
    lastg: [f32; crate::NB_BANDS],
    rnn: crate::rnn::RnnState,
}

impl DenoiseState {
    /// A `DenoiseState` processes this many samples at a time.
    pub const FRAME_SIZE: usize = FRAME_SIZE;

    /// Creates a new `DenoiseState`.
    pub fn new() -> Box<DenoiseState> {
        Box::new(DenoiseState {
            analysis_mem: [0.0; FRAME_SIZE],
            cepstral_mem: [[0.0; NB_BANDS]; CEPS_MEM],
            mem_id: 0,
            synthesis_mem: [0.0; FRAME_SIZE],
            pitch_buf: [0.0; PITCH_BUF_SIZE],
            last_gain: 0.0,
            last_period: 0,
            mem_hp_x: [0.0; 2],
            lastg: [0.0; NB_BANDS],
            rnn: crate::rnn::RnnState::new(),
        })
    }

    /// Processes a chunk of samples.
    ///
    /// Both `output` and `input` should be slices of length `DenoiseState::FRAME_SIZE`.
    ///
    /// The current output of `process_frame` depends on the current input, but also on the
    /// preceding inputs. Because of this, you might prefer to discard the very first output; it
    /// will contain some fade-in artifacts.
    pub fn process_frame(&mut self, output: &mut [f32], input: &[f32]) -> f32 {
        process_frame(self, output, input)
    }
}

fn frame_analysis(state: &mut DenoiseState, x: &mut [Complex], ex: &mut [f32], input: &[f32]) {
    let mut buf = [0.0; WINDOW_SIZE];
    for i in 0..FRAME_SIZE {
        buf[i] = state.analysis_mem[i];
    }
    for i in 0..crate::FRAME_SIZE {
        buf[i + crate::FRAME_SIZE] = input[i];
        state.analysis_mem[i] = input[i];
    }
    crate::apply_window(&mut buf[..]);
    crate::forward_transform(x, &buf[..]);
    crate::compute_band_corr(ex, x, x);
}

fn compute_frame_features(
    state: &mut DenoiseState,
    x: &mut [Complex],
    p: &mut [Complex],
    ex: &mut [f32],
    ep: &mut [f32],
    exp: &mut [f32],
    features: &mut [f32],
    input: &[f32],
) -> usize {
    let mut ly = [0.0; NB_BANDS];
    let mut p_buf = [0.0; WINDOW_SIZE];
    // Apparently, PITCH_BUF_SIZE wasn't the best name...
    let mut pitch_buf = [0.0; PITCH_BUF_SIZE / 2];
    let mut tmp = [0.0; NB_BANDS];

    frame_analysis(state, x, ex, input);
    for i in 0..(PITCH_BUF_SIZE - FRAME_SIZE) {
        state.pitch_buf[i] = state.pitch_buf[i + FRAME_SIZE];
    }
    for i in 0..FRAME_SIZE {
        state.pitch_buf[PITCH_BUF_SIZE - FRAME_SIZE + i] = input[i];
    }

    crate::pitch_downsample(&state.pitch_buf[..], &mut pitch_buf);
    let pitch_idx = crate::pitch_search(
        &pitch_buf[(PITCH_MAX_PERIOD / 2)..],
        &pitch_buf,
        PITCH_FRAME_SIZE,
        PITCH_MAX_PERIOD - 3 * PITCH_MIN_PERIOD,
    );
    let pitch_idx = PITCH_MAX_PERIOD - pitch_idx;

    let (pitch_idx, gain) = crate::remove_doubling(
        &pitch_buf[..],
        PITCH_MAX_PERIOD,
        PITCH_MIN_PERIOD,
        PITCH_FRAME_SIZE,
        pitch_idx,
        state.last_period,
        state.last_gain,
    );
    state.last_period = pitch_idx;
    state.last_gain = gain;

    for i in 0..WINDOW_SIZE {
        p_buf[i] = state.pitch_buf[PITCH_BUF_SIZE - WINDOW_SIZE - pitch_idx + i];
    }
    crate::apply_window(&mut p_buf[..]);
    crate::forward_transform(p, &p_buf[..]);
    crate::compute_band_corr(ep, p, p);
    crate::compute_band_corr(exp, x, p);
    for i in 0..NB_BANDS {
        exp[i] /= (0.001 + ex[i] * ep[i]).sqrt();
    }
    crate::dct(&mut tmp[..], exp);
    for i in 0..NB_DELTA_CEPS {
        features[NB_BANDS + 2 * NB_DELTA_CEPS + i] = tmp[i];
    }

    features[NB_BANDS + 2 * NB_DELTA_CEPS] -= 1.3;
    features[NB_BANDS + 2 * NB_DELTA_CEPS + 1] -= 0.9;
    features[NB_BANDS + 3 * NB_DELTA_CEPS] = 0.01 * (pitch_idx as f32 - 300.0);
    let mut log_max = -2.0;
    let mut follow = -2.0;
    let mut e = 0.0;
    for i in 0..NB_BANDS {
        ly[i] = (1e-2 + ex[i]).log10().max(log_max - 7.0).max(follow - 1.5);
        log_max = log_max.max(ly[i]);
        follow = (follow - 1.5).max(ly[i]);
        e += ex[i];
    }

    if e < 0.04 {
        /* If there's no audio, avoid messing up the state. */
        for i in 0..NB_FEATURES {
            features[i] = 0.0;
        }
        return 1;
    }
    crate::dct(features, &ly[..]);
    features[0] -= 12.0;
    features[1] -= 4.0;
    let ceps_0_idx = state.mem_id;
    let ceps_1_idx = if state.mem_id < 1 {
        CEPS_MEM + state.mem_id - 1
    } else {
        state.mem_id - 1
    };
    let ceps_2_idx = if state.mem_id < 2 {
        CEPS_MEM + state.mem_id - 2
    } else {
        state.mem_id - 2
    };

    for i in 0..NB_BANDS {
        state.cepstral_mem[ceps_0_idx][i] = features[i];
    }
    state.mem_id += 1;

    let ceps_0 = &state.cepstral_mem[ceps_0_idx];
    let ceps_1 = &state.cepstral_mem[ceps_1_idx];
    let ceps_2 = &state.cepstral_mem[ceps_2_idx];
    for i in 0..NB_DELTA_CEPS {
        features[i] = ceps_0[i] + ceps_1[i] + ceps_2[i];
        features[NB_BANDS + i] = ceps_0[i] - ceps_2[i];
        features[NB_BANDS + NB_DELTA_CEPS + i] = ceps_0[i] - 2.0 * ceps_1[i] + ceps_2[i];
    }

    /* Spectral variability features. */
    let mut spec_variability = 0.0;
    if state.mem_id == CEPS_MEM {
        state.mem_id = 0;
    }
    for i in 0..CEPS_MEM {
        let mut min_dist = 1e15f32;
        for j in 0..CEPS_MEM {
            let mut dist = 0.0;
            for k in 0..NB_BANDS {
                let tmp = state.cepstral_mem[i][k] - state.cepstral_mem[j][k];
                dist += tmp * tmp;
            }
            if j != i {
                min_dist = min_dist.min(dist);
            }
        }
        spec_variability += min_dist;
    }

    features[NB_BANDS + 3 * NB_DELTA_CEPS + 1] = spec_variability / CEPS_MEM as f32 - 2.1;

    return 0;
}

fn frame_synthesis(state: &mut DenoiseState, out: &mut [f32], y: &[Complex]) {
    let mut x = [0.0; WINDOW_SIZE];
    crate::inverse_transform(&mut x[..], y);
    crate::apply_window(&mut x[..]);
    for i in 0..FRAME_SIZE {
        out[i] = x[i] + state.synthesis_mem[i];
        state.synthesis_mem[i] = x[FRAME_SIZE + i];
    }
}

fn biquad(y: &mut [f32], mem: &mut [f32], x: &[f32], b: &[f32], a: &[f32]) {
    for i in 0..x.len() {
        let xi = x[i] as f64;
        let yi = (x[i] + mem[0]) as f64;
        mem[0] = (mem[1] as f64 + (b[0] as f64 * xi - a[0] as f64 * yi)) as f32;
        mem[1] = (b[1] as f64 * xi - a[1] as f64 * yi) as f32;
        y[i] = yi as f32;
    }
}

fn pitch_filter(
    x: &mut [Complex],
    p: &mut [Complex],
    ex: &[f32],
    ep: &[f32],
    exp: &[f32],
    g: &[f32],
) {
    let mut r = [0.0; NB_BANDS];
    let mut rf = [0.0; FREQ_SIZE];
    for i in 0..NB_BANDS {
        r[i] = if exp[i] > g[i] {
            1.0
        } else {
            let exp_sq = exp[i] * exp[i];
            let g_sq = g[i] * g[i];
            exp_sq * (1.0 - g_sq) / (0.001 + g_sq * (1.0 - exp_sq))
        };
        r[i] = 1.0_f32.min(0.0_f32.max(r[i])).sqrt();
        r[i] *= (ex[i] / (1e-8 + ep[i])).sqrt();
    }
    crate::interp_band_gain(&mut rf[..], &r[..]);
    for i in 0..FREQ_SIZE {
        x[i] += rf[i] * p[i];
    }

    let mut new_e = [0.0; NB_BANDS];
    crate::compute_band_corr(&mut new_e[..], x, x);
    let mut norm = [0.0; NB_BANDS];
    let mut normf = [0.0; FREQ_SIZE];
    for i in 0..NB_BANDS {
        norm[i] = (ex[i] / (1e-8 + new_e[i])).sqrt();
    }
    crate::interp_band_gain(&mut normf[..], &norm[..]);
    for i in 0..FREQ_SIZE {
        x[i] *= normf[i];
    }
}

fn process_frame(state: &mut DenoiseState, output: &mut [f32], input: &[f32]) -> f32 {
    let mut x_freq = [Complex::from(0.0); FREQ_SIZE];
    let mut p = [Complex::from(0.0); WINDOW_SIZE];
    let mut x_time = [0.0; FRAME_SIZE];
    let mut ex = [0.0; NB_BANDS];
    let mut ep = [0.0; NB_BANDS];
    let mut exp = [0.0; NB_BANDS];
    let mut features = [0.0; NB_FEATURES];
    let mut g = [0.0; NB_BANDS];
    let mut gf = [1.0; FREQ_SIZE];
    let a_hp = [-1.99599, 0.99600];
    let b_hp = [-2.0, 1.0];
    let mut vad_prob = [0.0];

    biquad(
        &mut x_time[..],
        &mut state.mem_hp_x[..],
        input,
        &b_hp[..],
        &a_hp[..],
    );
    let silence = compute_frame_features(
        state,
        &mut x_freq[..],
        &mut p[..],
        &mut ex[..],
        &mut ep[..],
        &mut exp[..],
        &mut features[..],
        &x_time[..],
    );
    if silence == 0 {
        crate::rnn::compute_rnn(&mut state.rnn, &mut g[..], &mut vad_prob[..], &features[..]);
        pitch_filter(
            &mut x_freq[..],
            &mut p[..],
            &mut ex[..],
            &mut ep[..],
            &mut exp[..],
            &mut g[..],
        );
        for i in 0..NB_BANDS {
            g[i] = g[i].max(0.6 * state.lastg[i]);
            state.lastg[i] = g[i];
        }
        crate::interp_band_gain(&mut gf[..], &g[..]);
        for i in 0..FREQ_SIZE {
            x_freq[i] *= gf[i];
        }
    }

    frame_synthesis(state, output, &x_freq[..]);
    vad_prob[0]
}
