//! Value fitting: given a key model, collect (inputs → output) samples by
//! probing, and search a template library for a BytesExpr that reproduces the
//! output. Ordered most-specific first; the first candidate that fits every
//! sample is handed to the verifier.
//!
//! Correctness note: candidate discovery may use masked/approximate numeric
//! reasoning, but every candidate is gated by `fits_all` — an exact byte
//! comparison of the fully-evaluated expression against the wasm's output on
//! every sample — before it is returned. Final soundness comes from
//! `verify::verify` on fresh randomized oracles (which include empty-key cases).

use super::host::{Oracle, Outcome, Prober};
use super::synth::{Feature, KeyModel, KeyShape, Rng};
use super::verify::{concrete_key, random_value, ValueDist};
use crate::plan::{eval_bytes, eval_num, BoolExpr, BytesExpr, NumExpr, PlanEnv};

struct SampleEnv<'a> {
    oracle: &'a Oracle,
    words: &'a [u128],
}
impl<'a> PlanEnv for SampleEnv<'a> {
    fn storage(&mut self, key: &[u8]) -> Vec<u8> {
        self.oracle.storage.get(key).cloned().unwrap_or_default()
    }
    fn height(&self) -> u64 {
        self.oracle.height
    }
    fn words(&self) -> &[u128] {
        self.words
    }
}

struct Sample {
    oracle: Oracle,
    args: Vec<u128>,
    output: Vec<u8>,
}

/// Collect successful samples with values drawn from `dist` (empty keys
/// included), random height and calldata.
fn collect_samples(
    prober: &Prober,
    opcode: u128,
    model: &KeyModel,
    n: usize,
    dist: ValueDist,
) -> Vec<Sample> {
    let mut rng = Rng::new(0x5A34D0 ^ opcode as u64 ^ (dist as u64) << 48);
    let mut samples = Vec::new();
    let mut attempts = 0;
    while samples.len() < n && attempts < n * 10 {
        attempts += 1;
        let mut oracle = Oracle::default();
        oracle.height = rng.next_u64() % 2_000_000;
        let args: Vec<u128> = (0..model.arg_count).map(|_| rng.value_u128()).collect();
        let calldata: Vec<u8> = args.iter().flat_map(|w| w.to_le_bytes()).collect();

        for key in &model.keys {
            if let Some(k) = concrete_key(key, &calldata) {
                let value = random_value(&mut rng, dist);
                if !value.is_empty() {
                    oracle.storage.insert(k, value);
                }
            }
        }

        match prober.run(opcode, &args, &oracle) {
            Ok(run) => match run.outcome {
                Outcome::Success(output) => samples.push(Sample {
                    oracle,
                    args,
                    output,
                }),
                _ => continue,
            },
            Err(_) => continue,
        }
    }
    samples
}

fn eval_on(expr: &BytesExpr, sample: &Sample) -> Option<Vec<u8>> {
    let mut env = SampleEnv {
        oracle: &sample.oracle,
        words: &sample.args,
    };
    eval_bytes(expr, &mut env, None).ok()
}

fn fits_all(expr: &BytesExpr, samples: &[Sample]) -> bool {
    samples
        .iter()
        .all(|s| eval_on(expr, s).as_ref() == Some(&s.output))
}

fn key_bytes(key: &KeyShape, sample: &Sample) -> Vec<u8> {
    let calldata: Vec<u8> = sample.args.iter().flat_map(|w| w.to_le_bytes()).collect();
    match concrete_key(key, &calldata) {
        Some(k) => sample.oracle.storage.get(&k).cloned().unwrap_or_default(),
        None => Vec::new(),
    }
}

/// Constant output width (all samples same length ≤ 16), for numeric encodings.
fn common_width(samples: &[Sample]) -> Option<u8> {
    let w = samples.first()?.output.len();
    if w == 0 || w > 16 {
        return None;
    }
    samples.iter().all(|s| s.output.len() == w).then_some(w as u8)
}

fn output_num(sample: &Sample) -> Option<u128> {
    if sample.output.len() > 16 {
        return None;
    }
    let mut buf = [0u8; 16];
    buf[..sample.output.len()].copy_from_slice(&sample.output);
    Some(u128::from_le_bytes(buf))
}

fn feature_val(feature: &Feature, sample: &Sample) -> Option<u128> {
    let mut env = SampleEnv {
        oracle: &sample.oracle,
        words: &sample.args,
    };
    eval_num(&feature.expr, &mut env, None).ok()
}

fn mask_for(width: u8) -> u128 {
    if width >= 16 {
        u128::MAX
    } else {
        (1u128 << (8 * width as u32)) - 1
    }
}

/// The main search. Returns a BytesExpr fitting every sample, or None.
pub fn fit_expr(
    prober: &Prober,
    opcode: u128,
    model: &KeyModel,
    dist: ValueDist,
) -> Option<BytesExpr> {
    let samples = collect_samples(prober, opcode, model, 40, dist);
    if samples.len() < 8 {
        return None; // not enough signal
    }

    // 0. Constant: every sample produces the same bytes regardless of storage,
    //    height, or calldata (a pure constant view, e.g. get_decimals → [8]).
    //    This is the ultimate fast path — no storage read at all. Verification
    //    against fresh oracles guards against a false constant.
    let first = &samples[0].output;
    if samples.iter().all(|s| &s.output == first) {
        return Some(BytesExpr::Const(first.clone()));
    }

    // 1. Raw passthrough: response is exactly one key's stored bytes.
    for key in &model.keys {
        let pt = BytesExpr::Storage(Box::new(key_expr(key)));
        if fits_all(&pt, &samples) {
            return Some(pt);
        }
    }

    // 2. Passthrough-with-default: `if the key is set, its bytes, else CONST`.
    //    (getName/getSymbol-style: stored override or a hardcoded default.)
    for key in &model.keys {
        if let Some(expr) = fit_passthrough_default(key, &samples) {
            if fits_all(&expr, &samples) {
                return Some(expr);
            }
        }
    }

    // Everything below fits a NUMBER, then encodes it at the observed width.
    let width = common_width(&samples)?;
    let mask = mask_for(width);
    let features = model.features();

    // 2b. Numeric passthrough with default: `if the key is set, its value
    //     re-encoded at the output width, else a constant`. (get_premium-style:
    //     a scalar getter that reads the stored value as a u128 and returns a
    //     hardcoded non-zero default when unset — distinct from raw passthrough
    //     because the stored bytes are reinterpreted and re-widened.)
    for key in &model.keys {
        if let Some(expr) = fit_numeric_passthrough_default(key, &samples, width) {
            if fits_all(&expr, &samples) {
                return Some(expr);
            }
        }
    }

    // Precompute masked feature values and masked targets per sample.
    let targets: Vec<u128> = samples
        .iter()
        .map(|s| output_num(s).map(|t| t & mask))
        .collect::<Option<_>>()?;
    let feats: Vec<Vec<u128>> = features
        .iter()
        .map(|f| {
            samples
                .iter()
                .map(|s| feature_val(f, s).map(|v| v & mask))
                .collect::<Option<Vec<_>>>()
        })
        .collect::<Option<_>>()?;

    let encode = |num: NumExpr| BytesExpr::Le {
        of: Box::new(num),
        width,
    };

    // 3. Single-feature identity (width-truncated): output == feature.
    for (i, f) in features.iter().enumerate() {
        if (0..samples.len()).all(|s| feats[i][s] == targets[s]) {
            let expr = encode(f.expr.clone());
            if fits_all(&expr, &samples) {
                return Some(expr);
            }
        }
    }

    // 4. Affine in one feature: output == feature * a + b.
    for (i, f) in features.iter().enumerate() {
        if let Some((a, b)) = fit_affine(&feats[i], &targets, mask) {
            let mut num = f.expr.clone();
            if a != 1 {
                num = NumExpr::Mul(Box::new(num), Box::new(NumExpr::Const(a)));
            }
            if b != 0 {
                num = NumExpr::Add(Box::new(num), Box::new(NumExpr::Const(b)));
            }
            let expr = encode(num);
            if fits_all(&expr, &samples) {
                return Some(expr);
            }
        }
    }

    // 5. Sum of all features, optionally divided by a small constant
    //    (averages, totals): output == (f0 + f1 + ...) / d.
    for d in [1u128, 2, 3, 4] {
        if (0..samples.len()).all(|s| {
            let sum = (0..features.len())
                .map(|i| feats[i][s])
                .fold(0u128, |a, b| a.wrapping_add(b));
            (sum / d) & mask == targets[s]
        }) {
            let mut sum = features[0].expr.clone();
            for f in &features[1..] {
                sum = NumExpr::Add(Box::new(sum), Box::new(f.expr.clone()));
            }
            let num = if d == 1 {
                sum
            } else {
                NumExpr::Div(Box::new(sum), Box::new(NumExpr::Const(d)))
            };
            let expr = encode(num);
            if fits_all(&expr, &samples) {
                return Some(expr);
            }
        }
    }

    // 6. Pairwise add/sub/mul of two features.
    for i in 0..features.len() {
        for j in 0..features.len() {
            if i == j {
                continue;
            }
            let ops: [(&str, fn(u128, u128) -> u128); 3] = [
                ("add", |a, b| a.wrapping_add(b)),
                ("sub", |a, b| a.wrapping_sub(b)),
                ("mul", |a, b| a.wrapping_mul(b)),
            ];
            for (name, op) in ops {
                if (0..samples.len()).all(|s| op(feats[i][s], feats[j][s]) & mask == targets[s]) {
                    let a = Box::new(features[i].expr.clone());
                    let b = Box::new(features[j].expr.clone());
                    let num = match name {
                        "add" => NumExpr::Add(a, b),
                        "sub" => NumExpr::Sub(a, b),
                        _ => NumExpr::Mul(a, b),
                    };
                    let expr = encode(num);
                    if fits_all(&expr, &samples) {
                        return Some(expr);
                    }
                }
            }
        }
    }

    None
}

/// Fit `if len(storage(key)) > 0 then storage(key) else CONST`. Requires at
/// least one empty-key sample and one non-empty sample to pin both branches.
fn fit_passthrough_default(key: &KeyShape, samples: &[Sample]) -> Option<BytesExpr> {
    let mut default: Option<Vec<u8>> = None;
    let mut saw_nonempty = false;

    for s in samples {
        let kb = key_bytes(key, s);
        if kb.is_empty() {
            // empty branch → output must be the same constant everywhere
            match &default {
                None => default = Some(s.output.clone()),
                Some(d) if d == &s.output => {}
                Some(_) => return None,
            }
        } else {
            // non-empty branch → output must equal the stored bytes
            if s.output != kb {
                return None;
            }
            saw_nonempty = true;
        }
    }

    let default = default?; // needs an empty-key observation
    if !saw_nonempty {
        return None;
    }

    Some(BytesExpr::If {
        // guard on the length of the stored VALUE, not the key
        cond: Box::new(BoolExpr::Gt(
            Box::new(NumExpr::Len(Box::new(BytesExpr::Storage(Box::new(key_expr(key)))))),
            Box::new(NumExpr::Const(0)),
        )),
        then: Box::new(BytesExpr::Storage(Box::new(key_expr(key)))),
        r#else: Box::new(BytesExpr::Const(default)),
    })
}

/// Fit `if len(storage(key)) > 0 then le(u(storage(key)), width) else CONST`.
/// The stored value is reinterpreted as a u128 and re-encoded at the output
/// width (so an 8-byte stored value becomes 16 output bytes), and the unset
/// case returns a recovered constant default. Requires both branches observed.
fn fit_numeric_passthrough_default(
    key: &KeyShape,
    samples: &[Sample],
    width: u8,
) -> Option<BytesExpr> {
    let mut default: Option<Vec<u8>> = None;
    let mut saw_nonempty = false;

    for s in samples {
        let kb = key_bytes(key, s);
        if kb.is_empty() {
            match &default {
                None => default = Some(s.output.clone()),
                Some(d) if d == &s.output => {}
                Some(_) => return None,
            }
        } else {
            // non-empty branch → output must equal le(u(stored), width)
            let take = kb.len().min(16);
            let mut buf = [0u8; 16];
            buf[..take].copy_from_slice(&kb[..take]);
            let n = u128::from_le_bytes(buf);
            let encoded = n.to_le_bytes()[..width as usize].to_vec();
            if s.output != encoded {
                return None;
            }
            saw_nonempty = true;
        }
    }

    let default = default?;
    if !saw_nonempty {
        return None;
    }

    Some(BytesExpr::If {
        cond: Box::new(BoolExpr::Gt(
            Box::new(NumExpr::Len(Box::new(BytesExpr::Storage(Box::new(key_expr(key)))))),
            Box::new(NumExpr::Const(0)),
        )),
        then: Box::new(BytesExpr::Le {
            of: Box::new(NumExpr::ULe(Box::new(BytesExpr::Storage(Box::new(key_expr(key)))))),
            width,
        }),
        r#else: Box::new(BytesExpr::Const(default)),
    })
}

fn key_expr(key: &KeyShape) -> BytesExpr {
    match key {
        KeyShape::Const(bytes) => BytesExpr::Const(bytes.clone()),
        KeyShape::Templated {
            prefix,
            suffix,
            cd_start,
            cd_len,
        } => {
            let mut parts = Vec::new();
            if !prefix.is_empty() {
                parts.push(BytesExpr::Const(prefix.clone()));
            }
            parts.push(BytesExpr::Calldata {
                start: *cd_start,
                len: Some(*cd_len),
            });
            if !suffix.is_empty() {
                parts.push(BytesExpr::Const(suffix.clone()));
            }
            BytesExpr::Concat(parts)
        }
    }
}

/// Fit output == x * a + b (mod 2^width) with small integer a, from two rows
/// with distinct x, then check all. Returns (a, b), a ≥ 1, rejecting the
/// trivial identity (handled earlier).
fn fit_affine(xs: &[u128], ys: &[u128], mask: u128) -> Option<(u128, u128)> {
    let mut basis = None;
    'outer: for i in 0..xs.len() {
        for j in i + 1..xs.len() {
            if xs[i] != xs[j] {
                basis = Some((i, j));
                break 'outer;
            }
        }
    }
    let (i, j) = basis?;
    let (xi, xj, yi, yj) = (xs[i], xs[j], ys[i], ys[j]);
    let (dx, dy) = if xi > xj {
        (xi - xj, yi.wrapping_sub(yj) & mask)
    } else {
        (xj - xi, yj.wrapping_sub(yi) & mask)
    };
    if dx == 0 || dy % dx != 0 {
        return None;
    }
    let a = dy / dx;
    if a == 0 || a > 1_000_000 {
        return None;
    }
    let b = yi.wrapping_sub(a.wrapping_mul(xi)) & mask;
    if a == 1 && b == 0 {
        return None;
    }
    for k in 0..xs.len() {
        if xs[k].wrapping_mul(a).wrapping_add(b) & mask != ys[k] {
            return None;
        }
    }
    Some((a, b))
}
