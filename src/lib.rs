use once_cell::sync::OnceCell;

mod denoise;
mod fft;
mod model;
mod rnn;

pub use denoise::DenoiseState;

fn inner_prod(xs: &[f32], ys: &[f32], n: usize) -> f32 {
    let mut sum0 = 0.0;
    let mut sum1 = 0.0;
    let mut sum2 = 0.0;
    let mut sum3 = 0.0;

    let n_4 = n - n % 4;
    for (x, y) in xs[..n_4].chunks_exact(4).zip(ys[..n_4].chunks_exact(4)) {
        sum0 += x[0] * y[0];
        sum1 += x[1] * y[1];
        sum2 += x[2] * y[2];
        sum3 += x[3] * y[3];
    }

    let mut sum = sum0 + sum1 + sum2 + sum3;
    for (&x, &y) in xs[n_4..n].iter().zip(&ys[n_4..n]) {
        sum += x * y;
    }
    sum
}

/// Does linear predictive coding (LPC) for a signal. The LPC coefficients are put into `lpc`,
/// which should have the same length as `ac`.
///
/// Very quick summary, mostly for my own understanding: the idea of LPC is to approximate a signal
/// x by shifted versions of it, so x[t] is approximately $\sum_i a_i x_{t-i}$, where the $a_i$ are
/// the LPC coefficients. This function determines the LPC coefficients using linear regression,
/// where the main observation is that this only requires a few auto-correlations. Therefore, the
/// function takes the autocorrelations as a parameter instead of the original signal.
///
/// This function solves the linear regression iteratively by first solving the smaller versions
/// (i.e., first solve the linear regression for one lag, then for two lags, and so on).
fn lpc(lpc: &mut [f32], ac: &[f32]) {
    let p = lpc.len();
    let mut error = ac[0];

    for b in lpc.iter_mut() {
        *b = 0.0;
    }

    if ac[0] == 0.0 {
        return;
    }

    for i in 0..p {
        // Sum up this iteration's reflection coefficient
        let mut rr = 0.0;
        for j in 0..i {
            rr += lpc[j] * ac[i - j];
        }
        rr += ac[i + 1];
        let r = -rr / error;
        // Update LPC coefficients and total error
        lpc[i] = r;
        for j in 0..((i + 1) / 2) {
            let tmp1 = lpc[j];
            let tmp2 = lpc[i - 1 - j];
            lpc[j] = tmp1 + r * tmp2;
            lpc[i - 1 - j] = tmp2 + r * tmp1;
        }

        error = error - r * r * error;
        // Bail out once we get 30 dB gain
        if error < 0.001 * ac[0] {
            return;
        }
    }
}

// Computes various terms of the cross-correlation between x and y (the number of terms to compute
// is determined by the size of `xcorr`).
fn pitch_xcorr(xs: &[f32], ys: &[f32], xcorr: &mut [f32]) {
    // The un-optimized version of this function is:
    //
    // for i in 0..xcorr.len() {
    //    xcorr[i] = xs.iter().zip(&ys[i..]).map(|(&x, &y)| x * y).sum();
    // }
    //
    // To optimize it, we unroll both the outer and inner loops four times each. This is a huge win
    // because it improves the pattern of access to ys. The compiler does a good job of vectorizing
    // the inner loop. (Maybe if we unrolled 8 times, it would be better on AVX?)

    let xcorr_len_4 = xcorr.len() - xcorr.len() % 4;
    let xs_len_4 = xs.len() - xs.len() % 4;

    for i in (0..xcorr_len_4).step_by(4) {
        let mut c0 = 0.0;
        let mut c1 = 0.0;
        let mut c2 = 0.0;
        let mut c3 = 0.0;

        let mut y0 = ys[i + 0];
        let mut y1 = ys[i + 1];
        let mut y2 = ys[i + 2];
        let mut y3 = ys[i + 3];

        for (x, y) in xs.chunks_exact(4).zip(ys[(i + 4)..].chunks_exact(4)) {
            c0 += x[0] * y0;
            c1 += x[0] * y1;
            c2 += x[0] * y2;
            c3 += x[0] * y3;

            y0 = y[0];
            c0 += x[1] * y1;
            c1 += x[1] * y2;
            c2 += x[1] * y3;
            c3 += x[1] * y0;

            y1 = y[1];
            c0 += x[2] * y2;
            c1 += x[2] * y3;
            c2 += x[2] * y0;
            c3 += x[2] * y1;

            y2 = y[2];
            c0 += x[3] * y3;
            c1 += x[3] * y0;
            c2 += x[3] * y1;
            c3 += x[3] * y2;

            y3 = y[3];
        }

        for j in xs_len_4..xs.len() {
            c0 += xs[j] * ys[i + 0 + j];
            c1 += xs[j] * ys[i + 1 + j];
            c2 += xs[j] * ys[i + 2 + j];
            c3 += xs[j] * ys[i + 3 + j];
        }
        xcorr[i + 0] = c0;
        xcorr[i + 1] = c1;
        xcorr[i + 2] = c2;
        xcorr[i + 3] = c3;
    }

    for i in xcorr_len_4..xcorr.len() {
        xcorr[i] = xs.iter().zip(&ys[i..]).map(|(&x, &y)| x * y).sum();
    }
}

/// Returns the indices with the largest and second-largest normalized auto-correlation.
///
/// `xcorr` is the autocorrelation of `ys`, taken with windows of length `len`.
///
/// To be a little more precise, the function that we're maximizing is xcorr[i] * xcorr[i],
/// divided by the squared norm of ys[i..(i+len)] (but with a bit of fudging to avoid dividing
/// by small things).
fn find_best_pitch(xcorr: &[f32], ys: &[f32], len: usize) -> (usize, usize) {
    let mut best_num = -1.0;
    let mut second_best_num = -1.0;
    let mut best_den = 0.0;
    let mut second_best_den = 0.0;
    let mut best_pitch = 0;
    let mut second_best_pitch = 1;
    let mut y_sq_norm = 1.0;
    for y in &ys[0..len] {
        y_sq_norm += y * y;
    }
    for (i, &corr) in xcorr.iter().enumerate() {
        if corr > 0.0 {
            let num = corr * corr;
            if num * second_best_den > second_best_num * y_sq_norm {
                if num * best_den > best_num * y_sq_norm {
                    second_best_num = best_num;
                    second_best_den = best_den;
                    second_best_pitch = best_pitch;
                    best_num = num;
                    best_den = y_sq_norm;
                    best_pitch = i;
                } else {
                    second_best_num = num;
                    second_best_den = y_sq_norm;
                    second_best_pitch = i;
                }
            }
        }
        y_sq_norm += ys[i + len] * ys[i + len] - ys[i] * ys[i];
        y_sq_norm = y_sq_norm.max(1.0);
    }
    (best_pitch, second_best_pitch)
}

// TODO: document this. There are some puzzles, commented below.
pub(crate) fn pitch_search(
    x_lp: &[f32],
    y: &[f32],
    len: usize,
    max_pitch: usize,
    x_lp4: &mut [f32],
    y_lp4: &mut [f32],
) -> usize {
    // It seems like only the first half of this is really used? The second half seems to always
    // stay zero.
    let mut xcorr = vec![0.0; max_pitch / 2];

    // It says "again", but this was only downsampled once? Also, it's downsampling only the first
    // half by 2.
    // Ah, this is called on the result of pitch_downsample.
    /* Downsample by 2 again */
    for j in 0..x_lp4.len() {
        x_lp4[j] = x_lp[2 * j];
    }
    for j in 0..y_lp4.len() {
        y_lp4[j] = y[2 * j];
    }
    pitch_xcorr(&x_lp4, &y_lp4, &mut xcorr[0..(max_pitch / 4)]);

    let (best_pitch, second_best_pitch) =
        find_best_pitch(&xcorr[0..(max_pitch / 4)], &y_lp4, len / 4);

    /* Finer search with 2x decimation */
    for i in 0..(max_pitch / 2) {
        xcorr[i] = 0.0;
        if (i as isize - 2 * best_pitch as isize).abs() > 2
            && (i as isize - 2 * second_best_pitch as isize).abs() > 2
        {
            continue;
        }
        xcorr[i] = inner_prod(&x_lp[..], &y[i..], len / 2).max(-1.0);
    }

    let (best_pitch, _) = find_best_pitch(&xcorr, &y, len / 2);

    /* Refine by pseudo-interpolation */
    let offset: isize = if best_pitch > 0 && best_pitch < (max_pitch / 2) - 1 {
        let a = xcorr[best_pitch - 1];
        let b = xcorr[best_pitch];
        let c = xcorr[best_pitch + 1];
        if c - a > 0.7 * (b - a) {
            1
        } else if a - c > 0.7 * (b - c) {
            -1
        } else {
            0
        }
    } else {
        0
    };
    (2 * best_pitch as isize - offset) as usize
}

fn fir5_in_place(xs: &mut [f32], num: &[f32]) {
    let num0 = num[0];
    let num1 = num[1];
    let num2 = num[2];
    let num3 = num[3];
    let num4 = num[4];

    let mut mem0 = 0.0;
    let mut mem1 = 0.0;
    let mut mem2 = 0.0;
    let mut mem3 = 0.0;
    let mut mem4 = 0.0;

    for x in xs {
        let out = *x + num0 * mem0 + num1 * mem1 + num2 * mem2 + num3 * mem3 + num4 * mem4;
        mem4 = mem3;
        mem3 = mem2;
        mem2 = mem1;
        mem1 = mem0;
        mem0 = *x;
        *x = out;
    }
}

/// Computes the autocorrelation of the sequence `x` (the number of terms to compute is determined
/// by the length of `ac`).
fn celt_autocorr(x: &[f32], ac: &mut [f32]) {
    let n = x.len();
    let lag = ac.len() - 1;
    let fast_n = n - lag;
    pitch_xcorr(&x[0..fast_n], x, ac);

    for k in 0..ac.len() {
        let mut d = 0.0;
        for i in (k + fast_n)..n {
            d += x[i] * x[i - k];
        }
        ac[k] += d;
    }
}

pub(crate) fn pitch_downsample(x: &[f32], x_lp: &mut [f32]) {
    let mut ac = [0.0; 5];
    let mut lpc_coeffs = [0.0; 4];
    let mut lpc_coeffs2 = [0.0; 5];

    for i in 1..(x.len() / 2) {
        x_lp[i] = ((x[2 * i - 1] + x[2 * i + 1]) / 2.0 + x[2 * i]) / 2.0;
    }
    x_lp[0] = (x[1] / 2.0 + x[0]) / 2.0;

    celt_autocorr(x_lp, &mut ac);

    // Noise floor -40 dB
    ac[0] *= 1.0001;
    // Lag windowing
    for i in 1..5 {
        ac[i] -= ac[i] * (0.008 * i as f32) * (0.008 * i as f32);
    }

    lpc(&mut lpc_coeffs, &ac);
    let mut tmp = 1.0;
    for i in 0..4 {
        tmp *= 0.9;
        lpc_coeffs[i] *= tmp;
    }
    // Add a zero
    lpc_coeffs2[0] = lpc_coeffs[0] + 0.8;
    lpc_coeffs2[1] = lpc_coeffs[1] + 0.8 * lpc_coeffs[0];
    lpc_coeffs2[2] = lpc_coeffs[2] + 0.8 * lpc_coeffs[1];
    lpc_coeffs2[3] = lpc_coeffs[3] + 0.8 * lpc_coeffs[2];
    lpc_coeffs2[4] = 0.8 * lpc_coeffs[3];

    fir5_in_place(x_lp, &lpc_coeffs2);
}

fn pitch_gain(xy: f32, xx: f32, yy: f32) -> f32 {
    xy / (1.0 + xx * yy).sqrt()
}

const SECOND_CHECK: [usize; 16] = [0, 0, 3, 2, 3, 2, 5, 2, 3, 2, 3, 2, 5, 2, 3, 2];

// TODO: document this.
fn remove_doubling(
    x: &[f32],
    mut max_period: usize,
    mut min_period: usize,
    mut n: usize,
    mut t0: usize,
    mut prev_period: usize,
    prev_gain: f32,
    yy_lookup: &mut [f32],
) -> (usize, f32) {
    let init_min_period = min_period;
    min_period /= 2;
    max_period /= 2;
    t0 /= 2;
    prev_period /= 2;
    n /= 2;
    t0 = t0.min(max_period - 1);

    let mut t = t0;

    // Note that because we can't index with negative numbers, the x in the C code is our
    // x[max_period..].
    let xx = inner_prod(&x[max_period..], &x[max_period..], n);
    let mut xy = inner_prod(&x[max_period..], &x[(max_period - t0)..], n);
    yy_lookup[0] = xx;

    let mut yy = xx;
    for i in 1..=max_period {
        yy += x[max_period - i] * x[max_period - i] - x[max_period + n - i] * x[max_period + n - i];
        yy_lookup[i] = yy.max(0.0);
    }

    yy = yy_lookup[t0];
    let mut best_xy = xy;
    let mut best_yy = yy;

    let g0 = pitch_gain(xy, xx, yy);
    let mut g = g0;

    // Look for any pitch at T/k */
    for k in 2..=15 {
        let t1 = (2 * t0 + k) / (2 * k);
        if t1 < min_period {
            break;
        }
        // Look for another strong correlation at t1b
        let t1b = if k == 2 {
            if t1 + t0 > max_period {
                t0
            } else {
                t0 + t1
            }
        } else {
            (2 * SECOND_CHECK[k] * t0 + k) / (2 * k)
        };
        xy = inner_prod(&x[max_period..], &x[(max_period - t1)..], n);
        let xy2 = inner_prod(&x[max_period..], &x[(max_period - t1b)..], n);
        xy = (xy + xy2) / 2.0;
        yy = (yy_lookup[t1] + yy_lookup[t1b]) / 2.0;

        let g1 = pitch_gain(xy, xx, yy);
        let cont = if (t1 as isize - prev_period as isize).abs() <= 1 {
            prev_gain
        } else if (t1 as isize - prev_period as isize).abs() <= 2 && 5 * k * k < t0 {
            prev_gain / 2.0
        } else {
            0.0
        };

        // Bias against very high pitch (very short period) to avoid false-positives due to
        // short-term correlation.
        let thresh = if t1 < 3 * min_period {
            (0.85 * g0 - cont).max(0.4)
        } else if t1 < 2 * min_period {
            (0.9 * g0 - cont).max(0.5)
        } else {
            (0.7 * g0 - cont).max(0.3)
        };
        if g1 > thresh {
            best_xy = xy;
            best_yy = yy;
            t = t1;
            g = g1;
        }
    }

    let best_xy = best_xy.max(0.0);
    let pg = if best_yy <= best_xy {
        1.0
    } else {
        best_xy / (best_yy + 1.0)
    };

    let mut xcorr = [0.0; 3];
    for k in 0..3 {
        xcorr[k] = inner_prod(&x[max_period..], &x[(max_period - (t + k - 1))..], n);
    }
    let offset: isize = if xcorr[2] - xcorr[0] > 0.7 * (xcorr[1] - xcorr[0]) {
        1
    } else if xcorr[0] - xcorr[2] > 0.7 * (xcorr[1] - xcorr[2]) {
        -1
    } else {
        0
    };

    let pg = pg.min(g);
    let t0 = (2 * t).wrapping_add(offset as usize).max(init_min_period);

    (t0, pg)
}

pub(crate) const FRAME_SIZE_SHIFT: usize = 2;
pub(crate) const FRAME_SIZE: usize = 120 << FRAME_SIZE_SHIFT;
pub(crate) const WINDOW_SIZE: usize = 2 * FRAME_SIZE;
pub(crate) const FREQ_SIZE: usize = FRAME_SIZE + 1;

pub(crate) const PITCH_MIN_PERIOD: usize = 60;
pub(crate) const PITCH_MAX_PERIOD: usize = 768;
pub(crate) const PITCH_FRAME_SIZE: usize = 960;
pub(crate) const PITCH_BUF_SIZE: usize = PITCH_MAX_PERIOD + PITCH_FRAME_SIZE;

pub(crate) const NB_BANDS: usize = 22;
pub(crate) const CEPS_MEM: usize = 8;
const NB_DELTA_CEPS: usize = 6;
pub(crate) const NB_FEATURES: usize = NB_BANDS + 3 * NB_DELTA_CEPS + 2;
const EBAND_5MS: [usize; 22] = [
    // 0  200 400 600 800  1k 1.2 1.4 1.6  2k 2.4 2.8 3.2  4k 4.8 5.6 6.8  8k 9.6 12k 15.6 20k*/
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 34, 40, 48, 60, 78, 100,
];
type Complex = num_complex::Complex<f32>;

pub(crate) fn compute_band_corr(out: &mut [f32], x: &[Complex], p: &[Complex]) {
    for y in out.iter_mut() {
        *y = 0.0;
    }

    for i in 0..(NB_BANDS - 1) {
        let band_size = (EBAND_5MS[i + 1] - EBAND_5MS[i]) << FRAME_SIZE_SHIFT;
        for j in 0..band_size {
            let frac = j as f32 / band_size as f32;
            let idx = (EBAND_5MS[i] << FRAME_SIZE_SHIFT) + j;
            let corr = x[idx].re * p[idx].re + x[idx].im * p[idx].im;
            out[i] += (1.0 - frac) * corr;
            out[i + 1] += frac * corr;
        }
    }
    out[0] *= 2.0;
    out[NB_BANDS - 1] *= 2.0;
}

fn interp_band_gain(out: &mut [f32], band_e: &[f32]) {
    for y in out.iter_mut() {
        *y = 0.0;
    }

    for i in 0..(NB_BANDS - 1) {
        let band_size = (EBAND_5MS[i + 1] - EBAND_5MS[i]) << FRAME_SIZE_SHIFT;
        for j in 0..band_size {
            let frac = j as f32 / band_size as f32;
            let idx = (EBAND_5MS[i] << FRAME_SIZE_SHIFT) + j;
            out[idx] = (1.0 - frac) * band_e[i] + frac * band_e[i + 1];
        }
    }
}

struct CommonState {
    window: [f32; WINDOW_SIZE],
    dct_table: [f32; NB_BANDS * NB_BANDS],
    fft: crate::fft::RealFft,
}

static COMMON: OnceCell<CommonState> = OnceCell::new();

fn common() -> &'static CommonState {
    if COMMON.get().is_none() {
        let pi = std::f64::consts::PI;
        let mut window = [0.0; WINDOW_SIZE];
        for i in 0..FRAME_SIZE {
            let sin = (0.5 * pi * (i as f64 + 0.5) / FRAME_SIZE as f64).sin();
            window[i] = (0.5 * pi * sin * sin).sin() as f32;
            window[WINDOW_SIZE - i - 1] = (0.5 * pi * sin * sin).sin() as f32;
        }

        let mut dct_table = [0.0; NB_BANDS * NB_BANDS];
        for i in 0..NB_BANDS {
            for j in 0..NB_BANDS {
                dct_table[i * NB_BANDS + j] =
                    ((i as f64 + 0.5) * j as f64 * pi / NB_BANDS as f64).cos() as f32;
                if j == 0 {
                    dct_table[i * NB_BANDS + j] *= 0.5f32.sqrt();
                }
            }
        }

        let fft = crate::fft::RealFft::new(WINDOW_SIZE);
        let _ = COMMON.set(CommonState {
            window,
            dct_table,
            fft,
        });
    }
    COMMON.get().unwrap()
}

/// A brute-force DCT (discrete cosine transform) of size NB_BANDS.
pub(crate) fn dct(out: &mut [f32], x: &[f32]) {
    let c = common();
    for i in 0..NB_BANDS {
        let mut sum = 0.0;
        for j in 0..NB_BANDS {
            sum += x[j] * c.dct_table[j * NB_BANDS + i];
        }
        out[i] = (sum as f64 * (2.0 / NB_BANDS as f64).sqrt()) as f32;
    }
}

fn zip3<I, J, K>(i: I, j: J, k: K) -> impl Iterator<Item = (I::Item, J::Item, K::Item)>
where
    I: IntoIterator,
    J: IntoIterator,
    K: IntoIterator,
{
    i.into_iter()
        .zip(j.into_iter().zip(k))
        .map(|(x, (y, z))| (x, y, z))
}

fn apply_window(output: &mut [f32], input: &[f32]) {
    let c = common();
    for (x, &y, &w) in zip3(output, input, &c.window[..]) {
        *x = y * w;
    }
}

fn apply_window_in_place(xs: &mut [f32]) {
    let c = common();
    for (x, &w) in xs.iter_mut().zip(&c.window[..]) {
        *x *= w;
    }
}

fn forward_transform(output: &mut [Complex], input: &mut [f32]) {
    let c = common();
    let mut buf = [Complex::new(0.0, 0.0); FREQ_SIZE];
    c.fft.forward(input, output, &mut buf[..]);

    // In the original RNNoise code, the forward transform is normalized and the inverse
    // tranform isn't. `rustfft` doesn't normalize either one, so we do it ourselves.
    let norm = 1.0 / WINDOW_SIZE as f32;
    for x in &mut output[..] {
        *x *= norm;
    }
}

fn inverse_transform(output: &mut [f32], input: &mut [Complex]) {
    let c = common();
    let mut buf = [Complex::new(0.0, 0.0); WINDOW_SIZE / 2];
    c.fft.inverse(input, output, &mut buf[..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_f32(bytes: &[u8]) -> Vec<f32> {
        let mut ret = Vec::with_capacity(bytes.len() / 2);
        for x in bytes.chunks_exact(2) {
            ret.push(i16::from_le_bytes([x[0], x[1]]) as f32);
        }
        ret
    }

    fn to_i16(bytes: &[u8]) -> Vec<i16> {
        let mut ret = Vec::with_capacity(bytes.len() / 2);
        for x in bytes.chunks_exact(2) {
            ret.push(i16::from_le_bytes([x[0], x[1]]));
        }
        ret
    }

    #[test]
    fn compare_to_reference() {
        let reference_input = to_f32(include_bytes!("../tests/testing.raw"));
        let reference_output = to_i16(include_bytes!("../tests/reference_output.raw"));
        let mut output = Vec::new();
        let mut out_buf = [0.0; FRAME_SIZE];
        let mut state = DenoiseState::new();
        let mut first = true;
        for chunk in reference_input.chunks_exact(FRAME_SIZE) {
            state.process_frame(&mut out_buf[..], chunk);
            if !first {
                output.extend_from_slice(&out_buf[..]);
            }
            first = false;
        }

        assert_eq!(output.len(), reference_output.len());
        let output = output.into_iter().map(|x| x as i16).collect::<Vec<_>>();
        let xx: f64 = reference_output.iter().map(|&n| n as f64 * n as f64).sum();
        let yy: f64 = output.iter().map(|&n| n as f64 * n as f64).sum();
        let xy: f64 = reference_output
            .into_iter()
            .zip(output)
            .map(|(n, m)| n as f64 * m as f64)
            .sum();
        let corr = xy / (xx.sqrt() * yy.sqrt());
        assert!((corr - 1.0).abs() < 1e-4);
    }
}
