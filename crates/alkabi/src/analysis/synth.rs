//! Plan synthesis by differential concolic probing.
//!
//! We treat a view method as an unknown pure function of (storage, height,
//! calldata) and recover it empirically:
//!
//!   1. KEY DISCOVERY — run against an empty oracle to see which keys the
//!      method reads, and whether the key set depends on calldata (probe with
//!      several random calldata vectors). Static keys → the key literals are
//!      constants; calldata-dependent keys → the key is a template with a
//!      calldata slice spliced in, whose layout we recover by diffing.
//!
//!   2. VALUE FITTING — with the key set known, vary the stored values (and
//!      height) across trials and fit the response as an expression over
//!      `u(storage(k))`, `height`, and calldata words. We try, cheapest first:
//!      identity passthrough, fixed-width integer projections, and affine /
//!      ratio combinations of the numeric inputs.
//!
//!   3. VERIFICATION lives in `verify.rs`: a candidate is only kept if it
//!      matches the wasm on many fresh randomized oracles.
//!
//! Anything not fit by the templates is simply left without a plan — the ABI
//! omits `plan` and the consumer falls back to simulate. Soundness comes from
//! step 3, so the template library can grow freely.

use super::host::{Oracle, Outcome, Prober, RunResult};
use crate::plan::{BytesExpr, NumExpr};
use anyhow::Result;

/// Deterministic PRNG (SplitMix64) — reproducible, no Date/rand dependency.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    pub fn next_u128(&mut self) -> u128 {
        ((self.next_u64() as u128) << 64) | self.next_u64() as u128
    }
    /// A "realistic" storage integer: mostly small, occasionally large.
    pub fn value_u128(&mut self) -> u128 {
        match self.next_u64() % 4 {
            0 => (self.next_u64() % 1000) as u128,
            1 => (self.next_u64() % 1_000_000_000) as u128,
            2 => self.next_u64() as u128,
            _ => self.next_u128(),
        }
    }
}

/// A storage key the method reads: either a fixed byte string, or a template
/// with one calldata-derived byte run spliced in at `insert`.
#[derive(Debug, Clone)]
pub enum KeyShape {
    Const(Vec<u8>),
    Templated {
        prefix: Vec<u8>,
        suffix: Vec<u8>,
        /// Byte range of flattened calldata spliced between prefix and suffix.
        cd_start: u32,
        cd_len: u32,
    },
}

impl KeyShape {
    fn key_expr(&self) -> BytesExpr {
        match self {
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
}

fn rand_args(rng: &mut Rng, n: usize) -> Vec<u128> {
    (0..n).map(|_| rng.value_u128()).collect()
}

/// Result of key discovery for one method.
pub struct KeyModel {
    pub keys: Vec<KeyShape>,
    pub arg_count: usize,
}

/// Discover the storage keys a view reads and whether they depend on calldata.
/// Returns None if the method is disqualified, traps unconditionally, reads a
/// number of keys that varies with stored data (data-dependent key sets are
/// out of scope for the current templates), or reads too many keys.
pub fn discover_keys(prober: &Prober, opcode: u128, arg_count: usize) -> Result<Option<KeyModel>> {
    let base = Oracle::default();

    // Empty-storage baseline across several random calldata vectors.
    let mut rng = Rng::new(0xA1CAB1_0000 ^ opcode as u64);
    let mut runs: Vec<(Vec<u128>, RunResult)> = Vec::new();
    for _ in 0..6 {
        let args = rand_args(&mut rng, arg_count);
        let run = prober.run(opcode, &args, &base)?;
        match &run.outcome {
            Outcome::Disqualified(_) => return Ok(None),
            _ => {}
        }
        runs.push((args, run));
    }

    // Key set must be stable in count across calldata (data-dependent key
    // fan-out, e.g. iterating a stored list, is not modeled). Zero keys is
    // fine — the method may be a pure function of height/calldata.
    let key_count = runs[0].1.keys.len();
    if key_count > 8 {
        return Ok(None);
    }
    if runs.iter().any(|(_, r)| r.keys.len() != key_count) {
        return Ok(None);
    }
    if key_count == 0 {
        return Ok(Some(KeyModel {
            keys: Vec::new(),
            arg_count,
        }));
    }

    // Confirm stability under stored values too: fill the baseline keys with
    // random data and re-check the count.
    let mut probe_oracle = base.clone();
    for (_, run) in &runs {
        for k in &run.keys {
            probe_oracle
                .storage
                .entry(k.clone())
                .or_insert_with(|| rng.next_u128().to_le_bytes().to_vec());
        }
    }
    for _ in 0..4 {
        let args = rand_args(&mut rng, arg_count);
        let run = prober.run(opcode, &args, &probe_oracle)?;
        if matches!(run.outcome, Outcome::Disqualified(_)) {
            return Ok(None);
        }
        if run.keys.len() != key_count {
            return Ok(None);
        }
    }

    // Classify each key slot: constant across calldata, or a template.
    let mut shapes = Vec::with_capacity(key_count);
    for slot in 0..key_count {
        let slot_keys: Vec<&Vec<u8>> = runs.iter().map(|(_, r)| &r.keys[slot]).collect();
        let all_same = slot_keys.iter().all(|k| *k == slot_keys[0]);
        if all_same {
            shapes.push(KeyShape::Const(slot_keys[0].clone()));
            continue;
        }
        match recover_template(&runs, slot, arg_count) {
            Some(shape) => shapes.push(shape),
            None => return Ok(None),
        }
    }

    Ok(Some(KeyModel {
        keys: shapes,
        arg_count,
    }))
}

/// Recover a templated key by locating the calldata bytes inside it. The key
/// equals prefix ++ (some contiguous run of the flattened calldata) ++ suffix.
fn recover_template(
    runs: &[(Vec<u128>, RunResult)],
    slot: usize,
    _arg_count: usize,
) -> Option<KeyShape> {
    let calldata = |args: &[u128]| -> Vec<u8> {
        args.iter().flat_map(|w| w.to_le_bytes()).collect()
    };

    let (args0, run0) = &runs[0];
    let key0 = &run0.keys[slot];
    let cd0 = calldata(args0);

    // Common prefix/suffix across all observed keys for this slot bounds the
    // variable region.
    let keys: Vec<&Vec<u8>> = runs.iter().map(|(_, r)| &r.keys[slot]).collect();
    if keys.iter().any(|k| k.len() != key0.len()) {
        // Variable-length insert: fall back to searching in key0 only, but
        // require equal length for the simple template model.
        return None;
    }
    let klen = key0.len();
    let mut pre = 0usize;
    while pre < klen && keys.iter().all(|k| k[pre] == key0[pre]) {
        pre += 1;
    }
    let mut suf = 0usize;
    while suf < klen - pre && keys.iter().all(|k| k[klen - 1 - suf] == key0[klen - 1 - suf]) {
        suf += 1;
    }
    let var_len = klen - pre - suf;
    if var_len == 0 {
        return None;
    }

    // The variable run in key0 must appear contiguously in cd0; find where.
    let needle = &key0[pre..pre + var_len];
    let cd_start = find_subslice(&cd0, needle)?;

    let shape = KeyShape::Templated {
        prefix: key0[..pre].to_vec(),
        suffix: key0[pre + var_len..].to_vec(),
        cd_start: cd_start as u32,
        cd_len: var_len as u32,
    };

    // Validate the recovered template reproduces every observed key.
    for (args, run) in runs {
        let cd = calldata(args);
        if cd_start + var_len > cd.len() {
            return None;
        }
        let rebuilt: Vec<u8> = key0[..pre]
            .iter()
            .chain(cd[cd_start..cd_start + var_len].iter())
            .chain(key0[pre + var_len..].iter())
            .copied()
            .collect();
        if &rebuilt != &run.keys[slot] {
            return None;
        }
    }

    Some(shape)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

/// Numeric inputs a fitted expression may combine, with how to rebuild each as
/// a plan NumExpr.
#[derive(Clone)]
pub struct Feature {
    pub name: String,
    pub expr: NumExpr,
}

impl KeyModel {
    /// The numeric features available to value-fitting: u(storage(key)) for
    /// each key, the calldata words, and height.
    pub fn features(&self) -> Vec<Feature> {
        let mut features = Vec::new();
        for (i, key) in self.keys.iter().enumerate() {
            features.push(Feature {
                name: format!("s{}", i),
                expr: NumExpr::ULe(Box::new(BytesExpr::Storage(Box::new(key.key_expr())))),
            });
        }
        for w in 0..self.arg_count {
            features.push(Feature {
                name: format!("w{}", w),
                expr: NumExpr::Word(w as u32),
            });
        }
        features.push(Feature {
            name: "h".to_string(),
            expr: NumExpr::Height,
        });
        features
    }

    /// The single-key passthrough expression (response == raw storage bytes).
    pub fn passthrough(&self) -> Option<BytesExpr> {
        if self.keys.len() == 1 {
            Some(BytesExpr::Storage(Box::new(self.keys[0].key_expr())))
        } else {
            None
        }
    }
}
