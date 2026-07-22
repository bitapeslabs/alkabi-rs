//! Differential verification: a synthesized plan is trustworthy only if it
//! reproduces the wasm's output across many fresh randomized oracles. This is
//! the soundness backstop — the synthesizer may propose anything; nothing ships
//! without surviving here.

use super::host::{Oracle, Outcome, Prober};
use super::synth::{KeyModel, KeyShape, Rng};
use crate::plan::{eval_plan, BytesExpr, Plan, PlanEnv};
use std::collections::BTreeMap;

/// A PlanEnv backed by a concrete oracle + calldata (mirrors the probe host's
/// world exactly, so plan and wasm see identical inputs).
struct OracleEnv<'a> {
    oracle: &'a Oracle,
    words: &'a [u128],
}

impl<'a> PlanEnv for OracleEnv<'a> {
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

/// The storage value-width distribution to probe with. Alkanes' StoragePointer
/// writes fixed-width integers (`set_value::<u64>` → 8 bytes, `::<u128>` → 16)
/// or raw byte/UTF-8 blobs, and an unset key reads as empty. Integer-typed and
/// string-typed getters need different distributions to fit (a binary blob
/// makes a `from_utf8` getter trap; a short/ascii blob makes an integer getter
/// take a `len < N` guard branch), so we probe each archetype separately and
/// VERIFY each candidate against the same distribution it was fit on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueDist {
    /// Empty, 8-byte, and 16-byte little-endian integers.
    Integer,
    /// Empty and printable-ASCII/UTF-8 blobs.
    Stringy,
}

impl ValueDist {
    pub const ALL: [ValueDist; 2] = [ValueDist::Integer, ValueDist::Stringy];
}

/// A storage value from the given distribution. The EMPTY value (a missing
/// key) is always in play — it exercises default-fallback branches, and getting
/// it into verification is what stops a bare-passthrough plan from shipping for
/// a method that actually returns a default when its key is unset.
pub(super) fn random_value(rng: &mut Rng, dist: ValueDist) -> Vec<u8> {
    match dist {
        ValueDist::Integer => match rng.next_u64() % 4 {
            0 => Vec::new(),                                            // unset
            1 => (rng.next_u64() as u128).to_le_bytes()[..8].to_vec(), // u64
            _ => rng.value_u128().to_le_bytes().to_vec(),              // u128
        },
        ValueDist::Stringy => match rng.next_u64() % 3 {
            0 => Vec::new(), // unset
            _ => {
                let n = (rng.next_u64() % 20 + 1) as usize;
                (0..n).map(|_| (rng.next_u64() % 94) as u8 + b'!').collect()
            }
        },
    }
}

/// Concrete key bytes for a case's calldata (None if calldata is too short).
pub(super) fn concrete_key(key: &KeyShape, calldata: &[u8]) -> Option<Vec<u8>> {
    Some(match key {
        KeyShape::Const(bytes) => bytes.clone(),
        KeyShape::Templated {
            prefix,
            suffix,
            cd_start,
            cd_len,
        } => {
            let start = *cd_start as usize;
            let end = start + *cd_len as usize;
            if end > calldata.len() {
                return None;
            }
            prefix
                .iter()
                .chain(calldata[start..end].iter())
                .chain(suffix.iter())
                .copied()
                .collect()
        }
    })
}

/// Build a random oracle populating the model's keys (empty values included),
/// with random height and calldata.
fn random_case(model: &KeyModel, rng: &mut Rng, dist: ValueDist) -> (Oracle, Vec<u128>) {
    let mut oracle = Oracle::default();
    oracle.height = rng.next_u64() % 2_000_000;

    let args: Vec<u128> = (0..model.arg_count).map(|_| rng.value_u128()).collect();
    let calldata: Vec<u8> = args.iter().flat_map(|w| w.to_le_bytes()).collect();

    for key in &model.keys {
        if let Some(concrete) = concrete_key(key, &calldata) {
            let value = random_value(rng, dist);
            if !value.is_empty() {
                oracle.storage.insert(concrete, value);
            }
            // empty value == leave key unset (missing == zero-length in host)
        }
    }

    (oracle, args)
}

/// Run `trials` randomized differential checks. Returns the number passed if
/// all passed; None if any disagreed (with the wasm's Success outcome) — a
/// single mismatch rejects the plan.
pub fn verify(
    prober: &Prober,
    opcode: u128,
    model: &KeyModel,
    expr: &BytesExpr,
    trials: u32,
    seed: u64,
    dist: ValueDist,
) -> Option<u32> {
    let plan = Plan {
        expr: expr.clone(),
        trials: 0,
    };
    let mut rng = Rng::new(seed);
    let mut checked = 0u32;

    // Attempt more cases than needed so disqualified/trap cases (skipped) still
    // let us reach `trials` genuine comparisons.
    let mut attempts = 0u32;
    let max_attempts = trials.saturating_mul(8).max(trials + 32);

    while checked < trials && attempts < max_attempts {
        attempts += 1;
        let (oracle, args) = random_case(model, &mut rng, dist);
        let run = match prober.run(opcode, &args, &oracle) {
            Ok(run) => run,
            Err(_) => return None,
        };
        let expected = match run.outcome {
            Outcome::Success(data) => data,
            // Traps/disqualifications on a given case aren't comparisons; the
            // plan only needs to match successful executions. Skip.
            _ => continue,
        };

        let mut env = OracleEnv {
            oracle: &oracle,
            words: &args,
        };
        match eval_plan(&plan, &mut env) {
            Ok(got) if got == expected => checked += 1,
            _ => return None,
        }
    }

    if checked >= trials {
        Some(checked)
    } else {
        None
    }
}

/// Convenience: an empty storage plus specified keys, used by unit tests.
#[allow(dead_code)]
pub fn oracle_with(pairs: &[(&[u8], &[u8])]) -> Oracle {
    let mut oracle = Oracle::default();
    oracle.storage = pairs
        .iter()
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
        .collect::<BTreeMap<_, _>>();
    oracle
}
