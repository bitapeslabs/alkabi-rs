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
use crate::plan::{eval_bytes, BoolExpr, BytesExpr, PlanEnv};
use interp::{Interp, LiftEnv, LiftOutcome};
use module::Module;
use std::cell::RefCell;
use std::collections::HashMap;

/// Per-key storage value width (in bytes) discovered during lifting. Alkanes
/// contracts store scalars as fixed-width little-endian; a `u64`-typed key
/// deserialized from a 16-byte value (or a `u128` key from 8 bytes) hits a
/// `try_into().unwrap()` panic. The discovered widths let both the lifter and
/// the verifier hand each key a value it can actually decode.
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
/// alongside the per-key storage widths it was lifted under (which the verifier
/// must reuse); the caller must verify the expr against the wasm before shipping.
///
/// The lifter runs the view concretely under many oracle *worlds* and merges the
/// results:
///
/// * **Per-key widths** are discovered greedily — every key starts at 16 bytes,
///   and a trap right after a key is read narrows it to 8 (a `u64` decoded from
///   16 bytes panics in the contract's deserializer). This alone is what lets
///   multi-scalar views — genesis(u64), epoch(u128), … — execute at all.
/// * **Branch merging**: different worlds take different control-flow branches
///   (e.g. a saturating/clamped path vs. the live path). Each run records the
///   symbolic predicates it branched on; [`merge_observations`] stitches runs
///   that diverge on one predicate into `if(pred, then, else)`. This recovers
///   views whose value depends on a `br_if`, not just a branchless `select`.
pub fn lift_view(module: &Module, opcode: u128, arg_count: usize) -> Vec<(BytesExpr, Widths)> {
    let (obs, widths) = collect_observations(module, opcode, arg_count);
    if obs.is_empty() {
        return Vec::new();
    }

    // Candidate plans, in the order the caller should try to verify them:
    //   1. each distinct single-world lift, most-frequently-observed first — a
    //      straight-line view lifts identically everywhere, so its one expr sits
    //      at the front and verifies immediately (this is the common case);
    //   2. the branch-merged expr, if the divergent worlds stitch together — the
    //      fallback for views whose value genuinely depends on a `br_if`.
    // Verification downstream rejects any candidate that doesn't match the wasm,
    // so a mis-fold in one world simply loses to a better candidate.
    let mut freq: Vec<(BytesExpr, usize)> = Vec::new();
    for (_, expr) in &obs {
        match freq.iter_mut().find(|(e, _)| e == expr) {
            Some((_, n)) => *n += 1,
            None => freq.push((expr.clone(), 1)),
        }
    }
    freq.sort_by(|a, b| b.1.cmp(&a.1));

    let branchy = freq.len() > 1;
    let mut candidates: Vec<(BytesExpr, Widths)> = Vec::new();
    for (expr, _) in freq.into_iter().take(6) {
        candidates.push((sym::simplify_bytes(expr), widths.clone()));
    }
    if let Some(merged) = merge_observations(&obs, 0) {
        let merged = sym::simplify_bytes(merged);
        if !candidates.iter().any(|(e, _)| *e == merged) {
            candidates.push((merged, widths.clone()));
        }
    }
    // A branchy view whose paths the single-run observations couldn't stitch
    // needs the full decision tree — recover it by concolic path forking. Gated
    // on branchiness so straight-line views (one distinct result) skip the cost.
    if branchy {
        for cand in explore_view(module, opcode, arg_count) {
            if !candidates.iter().any(|(e, _)| *e == cand.0) {
                candidates.push(cand);
            }
        }
    }
    candidates
}

/// Run the view under many oracle worlds (see [`lift_view`]) and return every
/// `(predicate trace, lifted expr)` observation plus the discovered per-key
/// widths. Widths persist across worlds so a narrowing found on one run carries
/// into the next.
fn collect_observations(
    module: &Module,
    opcode: u128,
    arg_count: usize,
) -> (Vec<(Vec<(BoolExpr, bool)>, BytesExpr)>, Widths) {
    const HEIGHT: u64 = 880_001;
    let widths: RefCell<Widths> = RefCell::new(HashMap::new());
    let cand_idx: RefCell<HashMap<Vec<u8>, usize>> = RefCell::new(HashMap::new());
    let mut obs: Vec<(Vec<(BoolExpr, bool)>, BytesExpr)> = Vec::new();

    // 1) Uniform archetypes — every key the same value. Small values drive the
    //    "far below tip" / clamped branches; near-tip values drive the live path.
    let archetypes: [u128; 6] = [
        7,
        1,
        0,
        HEIGHT as u128 - 10,
        HEIGHT as u128 - 105_000,
        HEIGHT as u128 / 2,
    ];
    for &base in &archetypes {
        if let Some(o) = run_world(module, opcode, arg_count, HEIGHT, |_| base, &widths, &cand_idx) {
            obs.push(o);
        }
    }
    // 2) Per-key varied worlds — distinct keys get distinct pseudo-random values
    //    (and a few heights), so key-vs-key comparisons flip both ways across
    //    worlds and the merge can see each side of a branch.
    for salt in 0..16u64 {
        let height = [HEIGHT, 200_000, 1_000_000, 50_000][(salt % 4) as usize];
        if let Some(o) = run_world(
            module,
            opcode,
            arg_count,
            height,
            |key| key_value(key, salt, height),
            &widths,
            &cand_idx,
        ) {
            obs.push(o);
        }
    }
    // 3) Small-height worlds with small values — every stored value in [0,height]
    //    against a low tip. This is the regime that keeps height-derived
    //    quantities *in range* (epoch small, `height - stored_height` positive
    //    and modest), so the "live" arithmetic paths — read a counter, subtract
    //    it saturating — are actually taken, both above and below the saturation
    //    threshold. Random large-height worlds almost never land this.
    for salt in 0..16u64 {
        let height = [64u64, 200, 1000, 8000, 100_000, 300_000][(salt as usize) % 6];
        if let Some(o) = run_world(
            module,
            opcode,
            arg_count,
            height,
            |key| (fnv(key, salt) as u128) % (height as u128 + 1),
            &widths,
            &cand_idx,
        ) {
            obs.push(o);
        }
    }
    // 4) Small stored values against a full-range tip. A stored counter well
    //    below the height-derived one is the "stale" regime — a contract that
    //    caches a height-derived quantity and recomputes it when the cache lags
    //    only takes its recompute branch here. Different `cap`s straddle the
    //    threshold both ways.
    for salt in 0..16u64 {
        let height = [HEIGHT, 500_000, 1_000_000, 210_001][(salt as usize) % 4];
        let cap = [3u128, 8, 20, 64, 200, 5000][(salt as usize) % 6];
        if let Some(o) = run_world(
            module,
            opcode,
            arg_count,
            height,
            |key| (fnv(key, salt.wrapping_add(777)) as u128) % (cap + 1),
            &widths,
            &cand_idx,
        ) {
            obs.push(o);
        }
    }
    let discovered = widths.borrow().clone();
    (obs, discovered)
}

/// FNV-1a of `key` mixed with `salt` — a cheap deterministic per-(key,world)
/// value source for the oracle sweeps.
fn fnv(key: &[u8], salt: u64) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64 ^ salt.wrapping_mul(0x1_0000_0001_b3);
    for &b in key {
        h = (h ^ b as u64).wrapping_mul(0x1_0000_0001_b3);
    }
    h
}

/// Run one oracle world with greedy per-key width discovery. Returns the lifted
/// `BytesExpr` and the (lowered) predicate trace, or `None` if the run can't be
/// made to complete-and-lower under any width assignment.
fn run_world<F: Fn(&[u8]) -> u128>(
    module: &Module,
    opcode: u128,
    arg_count: usize,
    height: u64,
    value_of: F,
    widths: &RefCell<Widths>,
    cand_idx: &RefCell<HashMap<Vec<u8>, usize>>,
) -> Option<(Vec<(BoolExpr, bool)>, BytesExpr)> {
    let order: RefCell<Vec<Vec<u8>>> = RefCell::new(Vec::new());
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
            value_of(key).to_le_bytes()[..w.min(16)].to_vec()
        };
        let (context, calldata_off) = context_bytes(opcode, arg_count);
        let env = LiftEnv {
            context,
            calldata_off,
            height,
            storage: &storage,
        };
        let mut interp = Interp::new(module, &env, 5_000_000);
        match interp.run_execute() {
            LiftOutcome::Data(sym) => {
                let expr = sym::simplify_bytes(sym.lower()?);
                // lower the predicate trace, dropping any predicate the plan IR
                // can't express (it can't be a merge split point anyway).
                let path: Vec<(BoolExpr, bool)> = interp
                    .take_path()
                    .into_iter()
                    .filter_map(|(b, t)| b.lower().map(|be| (be, t)))
                    .collect();
                return Some((path, expr));
            }
            LiftOutcome::Trap(_) => {
                // Narrow the width of the key read just before the trap — a
                // fixed-width deserializer panics immediately after its read.
                let k = order.borrow().last().cloned()?;
                let next = cand_idx.borrow().get(&k).map_or(1, |i| i + 1);
                match WIDTH_CANDIDATES.get(next) {
                    Some(&w) => {
                        cand_idx.borrow_mut().insert(k.clone(), next);
                        widths.borrow_mut().insert(k, w);
                    }
                    None => return None, // exhausted widths → value trap, not width
                }
            }
            _ => return None, // Disqualified / Unsupported: retrying won't help
        }
    }
    None
}

/// Deterministic pseudo-random per-key value biased toward realistic magnitudes:
/// sometimes just below the tip (so a genesis/height-typed key yields a small,
/// live epoch), sometimes small, sometimes spread across the range.
fn key_value(key: &[u8], salt: u64, height: u64) -> u128 {
    let h = fnv(key, salt);
    match h % 3 {
        0 => height.saturating_sub((h >> 8) % 210_000) as u128, // near/below tip
        1 => ((h >> 8) % 1024) as u128,                         // small
        _ => ((h >> 8) as u128) % (height.max(1) as u128),      // spread
    }
}

/*───────────────────────── multi-path exploration ─────────────────────────*/

/// Recover the FULL decision tree of a branchy view (`if(p0, if(p1, …), …)`) by
/// bounded concolic path forking. A single concrete run only lifts one path;
/// here every symbolic `if`/`br_if` is walked both ways by deterministic
/// re-execution with forced branch directions ([`Interp::set_forced`]). Because
/// the plan is verified against the bytecode afterwards, any path whose forced
/// concrete state went inconsistent (or an infeasible predicate combination)
/// simply loses — an over-eager tree is rejected, never shipped.
///
/// Returns candidate `(tree, widths)` pairs for the caller to verify. Empty if
/// nothing lifted.
fn explore_view(module: &Module, opcode: u128, arg_count: usize) -> Vec<(BytesExpr, Widths)> {
    const HEIGHT: u64 = 880_001;
    // A DIVERSE oracle pool. No single concrete world reaches every branch (a
    // branch guarded by `epoch_start > tip` is dead where values are small, and
    // vice-versa), and forcing a branch against a world that contradicts it
    // traps. So exploration draws from a pool: at each fork, whichever oracle
    // takes the wanted direction *naturally* supplies the live run. The regimes
    // span small in-range values, large/overflowing values, and small-vs-large
    // tips so that both sides of every guard are covered by someone.
    let specs: &[(u64, u128)] = &[
        (HEIGHT, 8),
        (HEIGHT, 64),
        (HEIGHT, 1000),
        (HEIGHT, 3_000_000),
        (1_000_000, 20),
        (500_000, 5),
        (200, 50),
        (200, 4000),
        (1000, 8),
        (8000, 200),
        (300_000, 2_000_000),
        (2_000_000, 5),
    ];
    let mut pool: Vec<ExOracle> = specs
        .iter()
        .enumerate()
        .map(|(i, &(h, cap))| {
            let f: Box<dyn Fn(&[u8]) -> u128> =
                Box::new(move |key: &[u8]| (fnv(key, i as u64 ^ 0xa5a5) as u128) % (cap + 1));
            (h, f)
        })
        .collect();
    // Per-key binary-scale oracles: each key is independently tiny (a small
    // counter — drives "stale"/recompute branches) or above the tip (drives
    // saturation/overflow). Different splits put different keys on each side, so
    // combinations a single uniform cap can't produce (e.g. one counter stale
    // *and* another past the tip) are reached by some oracle.
    for split in 0..6u64 {
        let f: Box<dyn Fn(&[u8]) -> u128> = Box::new(move |key: &[u8]| {
            if fnv(key, split ^ 0x9e37) & 1 == 0 {
                (fnv(key, split) % 64) as u128
            } else {
                HEIGHT as u128 + (fnv(key, split) % 2_000_000) as u128
            }
        });
        pool.push((HEIGHT, f));
    }

    let widths: RefCell<Widths> = RefCell::new(HashMap::new());
    let cand_idx: RefCell<HashMap<Vec<u8>, usize>> = RefCell::new(HashMap::new());
    // Cap exploration near the shippable-tree bound: a view whose tree would run
    // past this has too many interacting paths to capture completely, so bail
    // rather than spend runs building something we'd discard anyway.
    let mut budget = MAX_TREE_NODES + 200;
    match explore(module, opcode, arg_count, &pool, Vec::new(), &widths, &cand_idx, &mut budget) {
        // A tree this large is a view with too many interacting paths to capture
        // completely (and would bloat the ABI even if it verified) — leave it to
        // simulate rather than probe-verify a monster.
        Some(tree) if tree_size(&tree) <= MAX_TREE_NODES => {
            vec![(sym::simplify_bytes(tree), widths.borrow().clone())]
        }
        _ => Vec::new(),
    }
}

/// Node budget for a shippable decision tree (bounds ABI size + verify cost).
const MAX_TREE_NODES: usize = 2000;

fn tree_size(e: &BytesExpr) -> usize {
    match e {
        BytesExpr::If { then, r#else, .. } => 1 + tree_size(then) + tree_size(r#else),
        _ => 1,
    }
}

/// One exploration oracle: a tip height and a per-key value source.
type ExOracle = (u64, Box<dyn Fn(&[u8]) -> u128>);

/// DFS one subtree: run the path under `forced`, and if a symbolic branch lies
/// beyond the forced prefix (the frontier), recurse into both of its sides and
/// combine as `if(frontier, then, else)`.
#[allow(clippy::too_many_arguments)]
fn explore(
    module: &Module,
    opcode: u128,
    arg_count: usize,
    pool: &[ExOracle],
    forced: Vec<bool>,
    widths: &RefCell<Widths>,
    cand_idx: &RefCell<HashMap<Vec<u8>, usize>>,
    budget: &mut usize,
) -> Option<BytesExpr> {
    if *budget == 0 {
        return None;
    }
    *budget -= 1;
    let (path, result) = run_path(module, opcode, arg_count, pool, &forced, widths, cand_idx)?;
    // No symbolic branch past the forced prefix → this path is a leaf.
    if path.len() <= forced.len() {
        return Some(result);
    }
    // The frontier branch (first unforced symbolic branch). If it can't be
    // expressed in the plan IR, don't fork — treat the run as a leaf.
    let Some(cond) = path[forced.len()].0.clone() else {
        return Some(result);
    };
    let mut ft = forced.clone();
    ft.push(true);
    let mut ff = forced;
    ff.push(false);
    let then = explore(module, opcode, arg_count, pool, ft, widths, cand_idx, budget);
    let els = explore(module, opcode, arg_count, pool, ff, widths, cand_idx, budget);
    match (then, els) {
        (Some(t), Some(f)) if t == f => Some(t), // branch didn't affect the result
        (Some(t), Some(f)) => Some(BytesExpr::If {
            cond: Box::new(cond),
            then: Box::new(t),
            r#else: Box::new(f),
        }),
        // A fork went dead under *every* oracle in the pool: genuinely infeasible
        // (no input reaches it) or unsupported. Substituting the live side would
        // fabricate a wrong tree (e.g. dropping a saturation guard), so bail.
        _ => None,
    }
}

/// Run one path under `forced`, trying each pool oracle in turn: a forced
/// direction that traps under one oracle's concrete state may be live under
/// another (the symbolic result is oracle-independent, so any live run is
/// authoritative). Returns the lowered predicate trace (position-aligned with
/// the forced indices) and the lifted result.
fn run_path(
    module: &Module,
    opcode: u128,
    arg_count: usize,
    pool: &[ExOracle],
    forced: &[bool],
    widths: &RefCell<Widths>,
    cand_idx: &RefCell<HashMap<Vec<u8>, usize>>,
) -> Option<(Vec<(Option<BoolExpr>, bool)>, BytesExpr)> {
    for (height, val) in pool {
        if let Some(r) = run_path_one(module, opcode, arg_count, *height, val, forced, widths, cand_idx)
        {
            return Some(r);
        }
    }
    None
}

/// Run one path under `forced` against a single oracle (with per-key width
/// discovery). `None` if it trapped / disqualified / couldn't lower.
#[allow(clippy::too_many_arguments)]
fn run_path_one<F: Fn(&[u8]) -> u128>(
    module: &Module,
    opcode: u128,
    arg_count: usize,
    height: u64,
    value_of: &F,
    forced: &[bool],
    widths: &RefCell<Widths>,
    cand_idx: &RefCell<HashMap<Vec<u8>, usize>>,
) -> Option<(Vec<(Option<BoolExpr>, bool)>, BytesExpr)> {
    let order: RefCell<Vec<Vec<u8>>> = RefCell::new(Vec::new());
    let max_attempts = 2 * WIDTH_CANDIDATES.len() + 12;
    for _ in 0..max_attempts {
        order.borrow_mut().clear();
        let storage = |key: &[u8]| -> Vec<u8> {
            order.borrow_mut().push(key.to_vec());
            let w = widths.borrow().get(key).copied().unwrap_or(WIDTH_CANDIDATES[0]);
            value_of(key).to_le_bytes()[..w.min(16)].to_vec()
        };
        let (context, calldata_off) = context_bytes(opcode, arg_count);
        let env = LiftEnv {
            context,
            calldata_off,
            height,
            storage: &storage,
        };
        let mut interp = Interp::new(module, &env, 5_000_000);
        interp.set_forced(forced.to_vec());
        match interp.run_execute() {
            LiftOutcome::Data(sym) => {
                let expr = sym::simplify_bytes(sym.lower()?);
                let path = interp
                    .take_path()
                    .into_iter()
                    .map(|(b, dir)| (b.lower(), dir))
                    .collect();
                return Some((path, expr));
            }
            LiftOutcome::Trap(_) => {
                let k = order.borrow().last().cloned()?;
                let next = cand_idx.borrow().get(&k).map_or(1, |i| i + 1);
                match WIDTH_CANDIDATES.get(next) {
                    Some(&w) => {
                        cand_idx.borrow_mut().insert(k.clone(), next);
                        widths.borrow_mut().insert(k, w);
                    }
                    None => return None,
                }
            }
            _ => return None,
        }
    }
    None
}

/// Merge per-world observations `(predicate trace, result)` into one expression.
///
/// If every result is identical, that single expression is the plan. Otherwise
/// find a predicate that appears in *every* trace and is taken both ways across
/// the set, partition on it, recursively merge each side, and combine as
/// `if(pred, then, else)`. Predicates that don't cleanly separate the results
/// are skipped; if none separates them, the set is unmergeable (`None`) and the
/// view falls back to simulate. Everything here is still verified downstream, so
/// an over-eager merge is caught rather than shipped.
fn merge_observations(obs: &[(Vec<(BoolExpr, bool)>, BytesExpr)], depth: usize) -> Option<BytesExpr> {
    if obs.is_empty() || depth > 16 {
        return None;
    }
    let first = &obs[0].1;
    if obs.iter().all(|(_, r)| r == first) {
        return Some(first.clone());
    }
    // candidate split predicates: the union of all predicates seen.
    let mut candidates: Vec<BoolExpr> = Vec::new();
    for (trace, _) in obs {
        for (p, _) in trace {
            if !candidates.contains(p) {
                candidates.push(p.clone());
            }
        }
    }
    for pred in &candidates {
        let mut t: Vec<(Vec<(BoolExpr, bool)>, BytesExpr)> = Vec::new();
        let mut f: Vec<(Vec<(BoolExpr, bool)>, BytesExpr)> = Vec::new();
        let mut usable = true;
        for (trace, result) in obs {
            match trace.iter().find(|(p, _)| p == pred).map(|(_, b)| *b) {
                Some(true) => t.push((strip(trace, pred), result.clone())),
                Some(false) => f.push((strip(trace, pred), result.clone())),
                None => {
                    usable = false; // predicate absent here → can't split on it
                    break;
                }
            }
        }
        if !usable || t.is_empty() || f.is_empty() {
            continue;
        }
        if let (Some(then), Some(els)) = (
            merge_observations(&t, depth + 1),
            merge_observations(&f, depth + 1),
        ) {
            if then == els {
                return Some(then); // predicate didn't actually affect the result
            }
            return Some(BytesExpr::If {
                cond: Box::new(pred.clone()),
                then: Box::new(then),
                r#else: Box::new(els),
            });
        }
    }
    None
}

fn strip(trace: &[(BoolExpr, bool)], pred: &BoolExpr) -> Vec<(BoolExpr, bool)> {
    trace.iter().filter(|(p, _)| p != pred).cloned().collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::NumExpr;

    // `stored_epoch < computed_epoch` — a representative divergence predicate.
    fn pred() -> BoolExpr {
        BoolExpr::Lt(Box::new(NumExpr::Height), Box::new(NumExpr::Const(100)))
    }
    fn c(b: &[u8]) -> BytesExpr {
        BytesExpr::Const(b.to_vec())
    }

    #[test]
    fn merge_stitches_two_branches() {
        // one predicate, taken both ways → if(pred, then, else)
        let obs = vec![
            (vec![(pred(), true)], c(b"A")),
            (vec![(pred(), false)], c(b"B")),
        ];
        let merged = merge_observations(&obs, 0).expect("should merge");
        assert_eq!(
            merged,
            BytesExpr::If {
                cond: Box::new(pred()),
                then: Box::new(c(b"A")),
                r#else: Box::new(c(b"B")),
            }
        );
    }

    #[test]
    fn merge_collapses_identical_results() {
        // every world agrees → single expr, no conditional (the straight-line case)
        let obs = vec![
            (vec![(pred(), true)], c(b"X")),
            (vec![(pred(), false)], c(b"X")),
            (vec![], c(b"X")),
        ];
        assert_eq!(merge_observations(&obs, 0), Some(c(b"X")));
    }

    #[test]
    fn merge_rejects_uncaptured_divergence() {
        // same predicate trace, different results → the split point wasn't
        // captured, so refuse to merge rather than fabricate a plan.
        let obs = vec![
            (vec![(pred(), false)], c(b"A")),
            (vec![(pred(), false)], c(b"B")),
        ];
        assert_eq!(merge_observations(&obs, 0), None);
    }
}
