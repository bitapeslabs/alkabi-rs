//! View-plan synthesis (feature `extract`): recover, from a compiled contract
//! wasm alone, pure-expression plans for view methods so consumers with a
//! batched storage-read API can evaluate them instead of simulating.
//!
//! The pipeline per view method:
//!   1. [`synth::discover_keys`] — which storage keys it reads, and whether
//!      keys are calldata-templated.
//!   2. [`fit::fit_expr`] — fit a BytesExpr over those keys / calldata /
//!      height to the observed outputs.
//!   3. [`verify::verify`] — accept only if the expr matches the wasm across
//!      many fresh randomized oracles.
//!
//! Every step is empirical and gated by (3), so unsupported methods simply get
//! no plan (the consumer falls back to simulate). Nothing here requires the
//! contract to be built with alkabi.

pub mod fit;
pub mod host;
pub mod synth;
pub mod verify;

use crate::abi::{AbiDocument, MethodKind};
use crate::plan::Plan;
use host::Prober;
use verify::ValueDist;

/// Tuning for plan synthesis.
#[derive(Debug, Clone, Copy)]
pub struct AnalysisConfig {
    /// Per-run fuel ceiling in the probe interpreter.
    pub fuel: u64,
    /// Randomized differential trials a plan must pass to ship.
    pub verify_trials: u32,
    /// Base seed for verification (kept fixed for reproducibility).
    pub verify_seed: u64,
}

impl Default for AnalysisConfig {
    fn default() -> Self {
        Self {
            fuel: 100_000_000,
            verify_trials: 128,
            verify_seed: 0xA1CAB1_0F17,
        }
    }
}

/// How many calldata words to assume for a view whose input arity is unknown
/// (normalized/legacy ABIs). Views mostly take a single AlkaneId (2 words) or
/// nothing; probing 4 covers the common cases without exploding the search.
fn assumed_arg_count(method: &crate::abi::AbiMethod) -> usize {
    match &method.input {
        Some(io) => crate::analysis::synth_input_words(&io.schema).unwrap_or(4),
        None => 0,
    }
}

/// Attempt to synthesize a plan for one view opcode. Returns None if the method
/// can't be reduced (disqualified host use, no fitting expression, or failed
/// verification).
pub fn synthesize_one(
    prober: &Prober,
    opcode: u128,
    arg_count: usize,
    config: &AnalysisConfig,
) -> Option<Plan> {
    let model = synth::discover_keys(prober, opcode, arg_count).ok()??;

    // Each value-width archetype is fit and verified against its own
    // distribution; the first that yields a verified plan wins.
    for dist in ValueDist::ALL {
        let Some(expr) = fit::fit_expr(prober, opcode, &model, dist) else {
            continue;
        };
        if let Some(trials) = verify::verify(
            prober,
            opcode,
            &model,
            &expr,
            config.verify_trials,
            config.verify_seed ^ opcode as u64,
            dist,
        ) {
            return Some(Plan { expr, trials });
        }
    }
    None
}

/// Synthesize plans for every view method in `document` and attach them. Errors
/// building the prober are fatal; per-method failures just leave `plan = None`.
pub fn attach_plans(
    document: &mut AbiDocument,
    wasm: &[u8],
    config: &AnalysisConfig,
) -> anyhow::Result<()> {
    let prober = Prober::new(wasm, config.fuel)?;

    for method in document.methods.iter_mut() {
        if method.kind != MethodKind::View || method.plan.is_some() {
            continue;
        }
        let arg_count = assumed_arg_count(method);
        if let Some(plan) = synthesize_one(&prober, method.opcode, arg_count, config) {
            method.plan = Some(plan);
        }
    }
    Ok(())
}

/// Best-effort calldata word count for a borsh/legacy input schema (only the
/// simple fixed-width shapes; anything else returns None → the caller probes a
/// default arity).
pub(crate) fn synth_input_words(schema: &crate::schema::Schema) -> Option<usize> {
    use crate::schema::Schema;
    match schema {
        Schema::Primitive(p) => Some(match *p {
            "u8" | "u16" | "u32" | "u64" | "u128" | "i8" | "i16" | "i32" | "i64" | "i128" => 1,
            _ => return None,
        }),
        Schema::Struct(fields) => {
            let mut total = 0;
            for (_, field) in fields {
                total += synth_input_words(field)?;
            }
            Some(total)
        }
        // AlkaneId and refs are two words in practice, but we can't resolve
        // refs here without the registry; the caller's default (4) covers it.
        _ => None,
    }
}
