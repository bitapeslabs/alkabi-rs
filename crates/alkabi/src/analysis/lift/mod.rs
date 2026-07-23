//! Concolic lifter: recover a plan for a view by symbolically executing its
//! wasm, rather than fitting a template. Covers arbitrary straight-line pure
//! computation over storage/height/calldata — anything the plan IR can express.
//!
//! One concrete path is executed with a storage oracle that returns non-empty
//! values (so storage-dependent branches are taken); the symbolic tags on the
//! resulting `response.data` bytes lower to a `BytesExpr`. Data-dependent
//! branches within the value computation collapse to that path's choice, so the
//! lifted plan is only kept if it survives verification — which, run against the
//! bytecode over many oracles, rejects any path-specific or width-wrong lift.

pub mod interp;
pub mod module;
pub mod sym;

use super::host::{Oracle, Outcome, Prober};
use super::synth::Rng;
use crate::plan::{eval_bytes, BytesExpr, PlanEnv};
use interp::{Interp, LiftEnv, LiftOutcome};
use module::Module;

/// The default oracle world for a lifting run: fixed context, and storage that
/// returns a fixed non-empty 16-byte value for every key (so "value present"
/// branches are taken and loads succeed).
fn context_bytes(opcode: u128, arg_count: usize) -> (Vec<u8>, usize) {
    // [myself.block, myself.tx, caller.block, caller.tx, vout, count] then
    // [opcode, inputs...] — all u128 LE, no incoming alkanes.
    let header: [u128; 6] = [2, 7777, 0, 0, 4, 0];
    let mut words: Vec<u128> = header.to_vec();
    let calldata_word_start = words.len() + 1; // after opcode
    words.push(opcode);
    for i in 0..arg_count {
        words.push((0x1000 + i) as u128);
    }
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let calldata_off = calldata_word_start * 16;
    (bytes, calldata_off)
}

/// Attempt to lift a plan for `opcode`. Returns the raw (unverified) BytesExpr;
/// the caller must verify it against the wasm before shipping.
///
/// A few storage-value strategies are tried in turn: for straight-line code the
/// symbolic result is identical regardless, but a view that panics on some
/// concrete values (e.g. an emission-table index that overflows when a stored
/// height is far from the tip) may lift cleanly under a different world.
pub fn lift_view(module: &Module, opcode: u128, arg_count: usize) -> Option<BytesExpr> {
    const HEIGHT: u64 = 880_001;
    // each strategy maps a requested key to a concrete value
    let strategies: [fn(&[u8]) -> Vec<u8>; 4] = [
        |_| 7u128.to_le_bytes().to_vec(),
        |_| 1u128.to_le_bytes().to_vec(),
        // values near the tip — avoids "blocks since genesis" overflow traps
        |_| (HEIGHT as u128 - 10).to_le_bytes().to_vec(),
        |_| 0u128.to_le_bytes().to_vec(),
    ];
    for storage in strategies {
        let (context, calldata_off) = context_bytes(opcode, arg_count);
        let env = LiftEnv {
            context,
            calldata_off,
            height: HEIGHT,
            storage: &storage,
        };
        let mut interp = Interp::new(module, &env, 5_000_000);
        if let LiftOutcome::Data(sym) = interp.run_execute() {
            if let Some(expr) = sym.lower().map(sym::simplify_bytes) {
                return Some(expr);
            }
        }
    }
    None
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

/// Verify a lifted plan against the wasm. Each trial: pick a random world,
/// discover the keys the method reads there, populate them with random values,
/// run the wasm, and compare to the plan evaluated over the same storage.
/// A single mismatch rejects the plan (this is what catches path-specific or
/// width-wrong lifts).
pub fn verify_lifted(
    prober: &Prober,
    opcode: u128,
    arg_count: usize,
    expr: &BytesExpr,
    trials: u32,
    seed: u64,
) -> Option<u32> {
    let mut rng = Rng::new(seed);
    let mut checked = 0u32;
    let mut attempts = 0u32;
    let max_attempts = trials.saturating_mul(8).max(trials + 32);

    while checked < trials && attempts < max_attempts {
        attempts += 1;
        let mut oracle = Oracle::default();
        oracle.height = rng.next_u64() % 2_000_000;
        let args: Vec<u128> = (0..arg_count).map(|_| rng.value_u128()).collect();

        // discover keys read with empty storage in this world
        let probe = prober.run(opcode, &args, &oracle).ok()?;
        if matches!(probe.outcome, Outcome::Disqualified(_)) {
            return None;
        }
        for k in &probe.keys {
            // random value, occasionally empty (to exercise unset reads)
            if rng.next_u64() % 5 != 0 {
                oracle
                    .storage
                    .entry(k.clone())
                    .or_insert_with(|| rng.value_u128().to_le_bytes().to_vec());
            }
        }

        let run = prober.run(opcode, &args, &oracle).ok()?;
        let expected = match run.outcome {
            Outcome::Success(d) => d,
            _ => continue,
        };

        let mut env = OracleEnv {
            oracle: &oracle,
            words: &args,
        };
        match eval_bytes(expr, &mut env, None) {
            Ok(got) if got == expected => checked += 1,
            _ => return None,
        }
    }

    (checked >= trials).then_some(checked)
}
