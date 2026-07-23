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
use std::cell::RefCell;
use std::collections::HashMap;

/// Per-key storage value width (bytes) discovered while lifting. Alkanes stores
/// scalars fixed-width little-endian; a `u64`-typed key deserialized from a
/// 16-byte value (or a `u128` key from 8) hits a `try_into().unwrap()` panic. The
/// discovered widths let both the lifter and the verifier hand each key a value
/// it can actually decode — needed the moment a view reads a `u64` counter and a
/// `u128` amount side by side.
pub type Widths = HashMap<Vec<u8>, usize>;

/// Storage scalar widths to try per key, most common first (`u128`, then `u64`).
const WIDTH_CANDIDATES: [usize; 2] = [16, 8];

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

/// Attempt to lift a plan for `opcode`. Returns the raw (unverified) BytesExpr
/// alongside the per-key storage widths it lifted under (which the verifier must
/// reuse); the caller must verify the expr against the wasm before shipping.
///
/// Two axes are searched. **Value archetypes** guard against views that panic on
/// some concrete values (an emission index overflowing when a stored height is
/// far from the tip); straight-line code lifts identically regardless. **Per-key
/// widths** are discovered greedily: every key starts at 16 bytes, and a trap
/// right after a key is read narrows that key to 8 (a `u64` decoded from 16 bytes
/// panics). This is what lets a multi-scalar view — genesis(u64) + amount(u128) —
/// execute at all instead of trapping in a deserializer.
pub fn lift_view(module: &Module, opcode: u128, arg_count: usize) -> Option<(BytesExpr, Widths)> {
    const HEIGHT: u64 = 880_001;
    let value_archetypes: [u128; 5] = [
        7,
        1,
        HEIGHT as u128 - 10,
        0,
        0x0100_0000_0000_0000_0000_0000_0000_0001,
    ];

    for &base in &value_archetypes {
        // key -> chosen width; key -> index into WIDTH_CANDIDATES already in use.
        let widths: RefCell<Widths> = RefCell::new(HashMap::new());
        let cand_idx: RefCell<HashMap<Vec<u8>, usize>> = RefCell::new(HashMap::new());
        let order: RefCell<Vec<Vec<u8>>> = RefCell::new(Vec::new());

        // enough attempts to narrow several distinct keys once each
        let max_attempts = 2 * WIDTH_CANDIDATES.len() + 12;
        for _ in 0..max_attempts {
            order.borrow_mut().clear();
            let storage = |key: &[u8]| -> Vec<u8> {
                order.borrow_mut().push(key.to_vec());
                let w = widths
                    .borrow()
                    .get(key)
                    .copied()
                    .unwrap_or(WIDTH_CANDIDATES[0]);
                base.to_le_bytes()[..w.min(16)].to_vec()
            };
            let (context, calldata_off) = context_bytes(opcode, arg_count);
            let env = LiftEnv {
                context,
                calldata_off,
                height: HEIGHT,
                storage: &storage,
            };
            let mut interp = Interp::new(module, &env, 5_000_000);
            match interp.run_execute() {
                LiftOutcome::Data(sym) => {
                    let lowered = sym.lower().map(sym::simplify_bytes);
                    let discovered = widths.borrow().clone();
                    if let Some(expr) = lowered {
                        return Some((expr, discovered));
                    }
                    break; // lowering failed for this archetype; try the next
                }
                LiftOutcome::Trap(_) => {
                    // Narrow the width of the key read just before the trap — a
                    // fixed-width deserializer panics immediately after its read.
                    let Some(k) = order.borrow().last().cloned() else {
                        break; // trapped before any storage read: width won't help
                    };
                    let next = cand_idx.borrow().get(&k).map_or(1, |i| i + 1);
                    match WIDTH_CANDIDATES.get(next) {
                        Some(&w) => {
                            cand_idx.borrow_mut().insert(k.clone(), next);
                            widths.borrow_mut().insert(k, w);
                        }
                        None => break, // exhausted widths for this key → value trap
                    }
                }
                _ => break, // Disqualified / Unsupported: retrying won't help
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
    widths: &Widths,
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
            // random value, occasionally empty (to exercise unset reads),
            // truncated to the key's discovered width so its fixed-width
            // deserializer doesn't panic (mirrors how the lift ran).
            if rng.next_u64() % 5 != 0 {
                let w = widths.get(k).copied().unwrap_or(WIDTH_CANDIDATES[0]).min(16);
                let val = rng.value_u128().to_le_bytes()[..w].to_vec();
                oracle.storage.entry(k.clone()).or_insert(val);
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
