//! Plan evaluation, JSON round-trip, and — when the clock-in wasm is present —
//! end-to-end synthesis + differential verification against the bytecode.

#![cfg(feature = "extract")]

use alkabi::plan::{eval_plan, parse_plan, BytesExpr, NumExpr, Plan, PlanEnv};
use std::collections::BTreeMap;

struct TestEnv {
    storage: BTreeMap<Vec<u8>, Vec<u8>>,
    height: u64,
    words: Vec<u128>,
}

impl PlanEnv for TestEnv {
    fn storage(&mut self, key: &[u8]) -> Vec<u8> {
        self.storage.get(key).cloned().unwrap_or_default()
    }
    fn height(&self) -> u64 {
        self.height
    }
    fn words(&self) -> &[u128] {
        &self.words
    }
}

fn env(pairs: &[(&[u8], &[u8])], height: u64, words: Vec<u128>) -> TestEnv {
    TestEnv {
        storage: pairs.iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect(),
        height,
        words,
    }
}

fn roundtrip(plan: &Plan) -> Plan {
    let json = plan.to_json();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let parsed = parse_plan(&value).unwrap();
    assert_eq!(parsed.to_json(), json, "plan JSON must round-trip");
    parsed
}

#[test]
fn passthrough_reads_storage() {
    let plan = Plan {
        expr: BytesExpr::Storage(Box::new(BytesExpr::Const(b"/last_note".to_vec()))),
        trials: 0,
    };
    let plan = roundtrip(&plan);
    let mut e = env(&[(b"/last_note", b"hello")], 100, vec![]);
    assert_eq!(eval_plan(&plan, &mut e).unwrap(), b"hello");
}

#[test]
fn le_of_height() {
    let plan = Plan {
        expr: BytesExpr::Le {
            of: Box::new(NumExpr::Height),
            width: 8,
        },
        trials: 0,
    };
    let plan = roundtrip(&plan);
    let mut e = env(&[], 880_001, vec![]);
    assert_eq!(eval_plan(&plan, &mut e).unwrap(), 880_001u64.to_le_bytes());
}

#[test]
fn le_u_of_storage_truncates_width() {
    // getCounter shape: Le(u(storage("/counter")), 8) — takes the low 8 bytes
    // even when the stored value is a full 16-byte u128.
    let plan = Plan {
        expr: BytesExpr::Le {
            of: Box::new(NumExpr::ULe(Box::new(BytesExpr::Storage(Box::new(
                BytesExpr::Const(b"/counter".to_vec()),
            ))))),
            width: 8,
        },
        trials: 0,
    };
    let plan = roundtrip(&plan);
    let value = 42u128.to_le_bytes();
    let mut e = env(&[(b"/counter", &value)], 0, vec![]);
    assert_eq!(eval_plan(&plan, &mut e).unwrap(), 42u64.to_le_bytes());

    // unset key → zero
    let mut empty = env(&[], 0, vec![]);
    assert_eq!(eval_plan(&plan, &mut empty).unwrap(), [0u8; 8]);
}

#[test]
fn conditional_with_default() {
    // getName shape: if len(storage("/name")) > 0 then storage else "oyl corp"
    use alkabi::plan::BoolExpr;
    let key = || BytesExpr::Const(b"/name".to_vec());
    let plan = Plan {
        expr: BytesExpr::If {
            cond: Box::new(BoolExpr::Gt(
                Box::new(NumExpr::Len(Box::new(BytesExpr::Storage(Box::new(key()))))),
                Box::new(NumExpr::Const(0)),
            )),
            then: Box::new(BytesExpr::Storage(Box::new(key()))),
            r#else: Box::new(BytesExpr::Const(b"oyl corp".to_vec())),
        },
        trials: 0,
    };
    let plan = roundtrip(&plan);
    let mut set = env(&[(b"/name", b"CUSTOM")], 0, vec![]);
    assert_eq!(eval_plan(&plan, &mut set).unwrap(), b"CUSTOM");
    let mut unset = env(&[], 0, vec![]);
    assert_eq!(eval_plan(&plan, &mut unset).unwrap(), b"oyl corp");
}

#[test]
fn templated_key_from_calldata() {
    // "/user/" ++ calldata[0..2] — an address-keyed getter.
    let plan = Plan {
        expr: BytesExpr::Storage(Box::new(BytesExpr::Concat(vec![
            BytesExpr::Const(b"/user/".to_vec()),
            BytesExpr::Calldata {
                start: 0,
                len: Some(2),
            },
        ]))),
        trials: 0,
    };
    let plan = roundtrip(&plan);
    // word 0 = 0x0201 → first two LE bytes [0x01, 0x02]
    let mut e = env(&[(b"/user/\x01\x02", b"balance!")], 0, vec![0x0201]);
    assert_eq!(eval_plan(&plan, &mut e).unwrap(), b"balance!");
}

#[test]
fn arithmetic_and_division() {
    // (u(a) + u(b)) / 2 — the "average of two counters" example.
    let sum = NumExpr::Add(
        Box::new(NumExpr::ULe(Box::new(BytesExpr::Storage(Box::new(
            BytesExpr::Const(b"/counter1".to_vec()),
        ))))),
        Box::new(NumExpr::ULe(Box::new(BytesExpr::Storage(Box::new(
            BytesExpr::Const(b"/counter2".to_vec()),
        ))))),
    );
    let plan = Plan {
        expr: BytesExpr::Le {
            of: Box::new(NumExpr::Div(Box::new(sum), Box::new(NumExpr::Const(2)))),
            width: 16,
        },
        trials: 0,
    };
    let plan = roundtrip(&plan);
    let mut e = env(
        &[
            (b"/counter1", &10u128.to_le_bytes()),
            (b"/counter2", &20u128.to_le_bytes()),
        ],
        0,
        vec![],
    );
    assert_eq!(eval_plan(&plan, &mut e).unwrap(), 15u128.to_le_bytes());
}

/// End-to-end: build clock-in fresh (if the toolchain is available) and confirm
/// synthesis recovers the expected plans, each surviving verification. Skipped
/// when the wasm isn't present so the suite stays hermetic.
#[test]
fn synthesizes_clockin_plans_if_wasm_present() {
    let wasm_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../alkabi-contracts/target/wasm32-unknown-unknown/release/clock_in.wasm"
    );
    let Ok(wasm) = std::fs::read(wasm_path) else {
        eprintln!("skipping: clock-in wasm not built at {}", wasm_path);
        return;
    };

    use alkabi::analysis::host::Prober;
    use alkabi::analysis::{synthesize_one, AnalysisConfig};

    let prober = Prober::new(&wasm, 100_000_000).unwrap();
    let config = AnalysisConfig {
        verify_trials: 64,
        ..AnalysisConfig::default()
    };

    // getCounter (104) and getHeight (102) must both reduce to plans.
    let counter = synthesize_one(&prober, 104, 0, &config);
    assert!(counter.is_some(), "getCounter should synthesize a plan");
    assert!(counter.unwrap().trials >= 64);

    let height = synthesize_one(&prober, 102, 0, &config);
    assert!(height.is_some(), "getHeight should synthesize a plan");
}
