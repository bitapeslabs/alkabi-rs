//! List-read plan synthesis.
//!
//! metashrew's `KeyValuePointer` stores a list as a `{base}/length` (u32) key
//! plus decimal-indexed `{base}/{i}` element keys, and `get_list()` reads the
//! length then each element. A view that concatenates such a list (e.g. frBTC's
//! `get_pending_payments`) has a *data-dependent* key set — the number of keys
//! grows with the stored length — so the generic synthesizer, which requires a
//! stable key set, rejects it.
//!
//! This module recognizes the pattern directly and emits a `loop` plan:
//!
//!   loop {
//!     count: u(storage( base ++ "/length" )),
//!     body:  storage( base ++ "/" ++ decimal(var) ),
//!   }
//!
//! where `base` may embed the block height (from `select_value(self.height())`)
//! as an 8-byte little-endian splice. As always, nothing ships unless it
//! reproduces the wasm across randomized differential trials — here with
//! oracles that populate real lists (random length + element values).

use super::host::{Oracle, Outcome, Prober};
use super::synth::Rng;
use super::AnalysisConfig;
use crate::plan::{eval_bytes, BytesExpr, NumExpr, Plan, PlanEnv};

const LENGTH_SUFFIX: &[u8] = b"/length";

/// A list base key, possibly with the block height spliced in.
struct BaseTemplate {
    prefix: Vec<u8>,
    /// When true, the 8-byte LE height sits between prefix and suffix.
    has_height: bool,
    suffix: Vec<u8>,
}

impl BaseTemplate {
    fn concrete(&self, height: u64) -> Vec<u8> {
        let mut out = self.prefix.clone();
        if self.has_height {
            out.extend_from_slice(&height.to_le_bytes());
            out.extend_from_slice(&self.suffix);
        }
        out
    }

    fn to_expr(&self) -> BytesExpr {
        if !self.has_height {
            return BytesExpr::Const(self.prefix.clone());
        }
        let mut parts = Vec::new();
        if !self.prefix.is_empty() {
            parts.push(BytesExpr::Const(self.prefix.clone()));
        }
        parts.push(BytesExpr::Le {
            of: Box::new(NumExpr::Height),
            width: 8,
        });
        if !self.suffix.is_empty() {
            parts.push(BytesExpr::Const(self.suffix.clone()));
        }
        BytesExpr::Concat(parts)
    }
}

/// Recover a base template from concrete base bytes: splice out the probe's
/// height if its 8-byte LE encoding appears contiguously.
fn templatize(base: &[u8], height: u64) -> BaseTemplate {
    let h8 = height.to_le_bytes();
    if let Some(pos) = base.windows(8).position(|w| w == h8) {
        BaseTemplate {
            prefix: base[..pos].to_vec(),
            has_height: true,
            suffix: base[pos + 8..].to_vec(),
        }
    } else {
        BaseTemplate {
            prefix: base.to_vec(),
            has_height: false,
            suffix: Vec::new(),
        }
    }
}

fn element_key(base: &[u8], i: u32) -> Vec<u8> {
    let mut k = base.to_vec();
    k.extend_from_slice(format!("/{}", i).as_bytes());
    k
}

fn length_key(base: &[u8]) -> Vec<u8> {
    let mut k = base.to_vec();
    k.extend_from_slice(LENGTH_SUFFIX);
    k
}

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

/// Attempt a list-read plan for a view. Returns None if the method isn't a
/// single concatenated list read, or the plan fails verification.
pub fn try_list_plan(
    prober: &Prober,
    opcode: u128,
    arg_count: usize,
    config: &AnalysisConfig,
) -> Option<Plan> {
    let base_oracle = Oracle::default();
    let no_args = vec![0u128; arg_count];

    // 1. Empty-storage probe: the method must read exactly one key, a
    //    `.../length` (length 0 → no elements read yet).
    let r0 = prober.run(opcode, &no_args, &base_oracle).ok()?;
    if !matches!(r0.outcome, Outcome::Success(_)) {
        return None;
    }
    if r0.keys.len() != 1 {
        return None;
    }
    let length = &r0.keys[0];
    if !length.ends_with(LENGTH_SUFFIX) {
        return None;
    }
    let base = length[..length.len() - LENGTH_SUFFIX.len()].to_vec();

    // 2. Set the length to a small count and confirm decimal-indexed element
    //    keys `{base}/0..{base}/{n-1}` are what get read.
    const PROBE_N: u32 = 3;
    let mut o = base_oracle.clone();
    o.storage
        .insert(length_key(&base), PROBE_N.to_le_bytes().to_vec());
    let r1 = prober.run(opcode, &no_args, &o).ok()?;
    if !matches!(r1.outcome, Outcome::Success(_)) {
        return None;
    }
    for i in 0..PROBE_N {
        let want = element_key(&base, i);
        if !r1.keys.iter().any(|k| *k == want) {
            return None;
        }
    }

    // 3. Recover the base template (height splice) and build the loop plan.
    let template = templatize(&base, base_oracle.height);
    let base_expr = template.to_expr();

    let length_expr = concat_with(&base_expr, BytesExpr::Const(LENGTH_SUFFIX.to_vec()));
    let element_expr = {
        let mut parts = expr_parts(&base_expr);
        parts.push(BytesExpr::Const(b"/".to_vec()));
        parts.push(BytesExpr::Decimal(Box::new(NumExpr::Var)));
        BytesExpr::Concat(parts)
    };

    let expr = BytesExpr::Loop {
        count: Box::new(NumExpr::ULe(Box::new(BytesExpr::Storage(Box::new(
            length_expr,
        ))))),
        body: Box::new(BytesExpr::Storage(Box::new(element_expr))),
    };

    // 4. Verify against the wasm with randomized, list-populated oracles.
    let trials = verify_list(
        prober,
        opcode,
        arg_count,
        &template,
        &expr,
        config.verify_trials,
        config.verify_seed ^ opcode as u64 ^ 0x1157,
    )?;

    Some(Plan { expr, trials })
}

fn expr_parts(base: &BytesExpr) -> Vec<BytesExpr> {
    match base {
        BytesExpr::Concat(parts) => parts.clone(),
        other => vec![other.clone()],
    }
}

fn concat_with(base: &BytesExpr, tail: BytesExpr) -> BytesExpr {
    let mut parts = expr_parts(base);
    parts.push(tail);
    BytesExpr::Concat(parts)
}

fn verify_list(
    prober: &Prober,
    opcode: u128,
    arg_count: usize,
    template: &BaseTemplate,
    expr: &BytesExpr,
    trials: u32,
    seed: u64,
) -> Option<u32> {
    let plan = Plan {
        expr: expr.clone(),
        trials: 0,
    };
    let mut rng = Rng::new(seed);
    let mut checked = 0u32;
    let mut attempts = 0u32;
    let max_attempts = trials.saturating_mul(8).max(trials + 32);

    while checked < trials && attempts < max_attempts {
        attempts += 1;

        let mut oracle = Oracle::default();
        oracle.height = rng.next_u64() % 2_000_000;
        let args: Vec<u128> = (0..arg_count).map(|_| rng.value_u128()).collect();

        // random small list at the height-specific base
        let base = template.concrete(oracle.height);
        let count = (rng.next_u64() % 6) as u32; // 0..5, includes empty
        if count > 0 {
            oracle
                .storage
                .insert(length_key(&base), count.to_le_bytes().to_vec());
            for i in 0..count {
                let vlen = (rng.next_u64() % 40 + 1) as usize;
                let val: Vec<u8> = (0..vlen).map(|_| (rng.next_u64() & 0xff) as u8).collect();
                oracle.storage.insert(element_key(&base, i), val);
            }
        }

        let run = prober.run(opcode, &args, &oracle).ok()?;
        let expected = match run.outcome {
            Outcome::Success(data) => data,
            _ => continue,
        };

        let mut env = OracleEnv {
            oracle: &oracle,
            words: &args,
        };
        match eval_bytes(&plan.expr, &mut env, None) {
            Ok(got) if got == expected => checked += 1,
            _ => return None,
        }
    }

    (checked >= trials).then_some(checked)
}
