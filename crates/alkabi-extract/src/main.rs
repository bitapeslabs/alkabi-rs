//! Thin CLI over `alkabi::extract`: reads a compiled contract wasm, extracts
//! its ABI (normalizing upstream-derive contracts), and writes `abi.json` +
//! `abi.ts` (an `as const` data file the TS side consumes for literal types).
//!
//! Usage: alkabi-extract <contract.wasm> [-o <out-dir>]   (out-dir defaults to ./abis)

use alkabi::analysis::{attach_plans, AnalysisConfig};
use alkabi::extract::{extract_meta_bytes, parse_abi_json};
use anyhow::{bail, Context, Result};
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut wasm_path: Option<PathBuf> = None;
    let mut out_dir = PathBuf::from("abis");
    let mut plans = false;
    let mut trials: u32 = AnalysisConfig::default().verify_trials;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" | "--out" => {
                let value = args.next().context("-o requires a directory argument")?;
                out_dir = PathBuf::from(value);
            }
            "--plans" => {
                plans = true;
            }
            "--trials" => {
                let value = args.next().context("--trials requires a number")?;
                trials = value.parse().context("--trials must be a number")?;
                plans = true;
            }
            "-h" | "--help" => {
                eprintln!(
                    "Usage: alkabi-extract <contract.wasm> [-o <out-dir>] [--plans] [--trials N]\n\
                     \n\
                     --plans      synthesize verified view plans (static fast-path) from the wasm\n\
                     --trials N   randomized verification trials per plan (default {}); implies --plans",
                    AnalysisConfig::default().verify_trials,
                );
                return Ok(());
            }
            other => {
                if wasm_path.is_some() {
                    bail!("Unexpected argument: {}", other);
                }
                wasm_path = Some(PathBuf::from(other));
            }
        }
    }

    let wasm_path = wasm_path.context("Usage: alkabi-extract <contract.wasm> [-o <out-dir>]")?;
    let wasm = std::fs::read(&wasm_path)
        .with_context(|| format!("Failed to read {}", wasm_path.display()))?;

    let meta = extract_meta_bytes(&wasm)?;
    let json = std::str::from_utf8(&meta).context("__meta returned invalid UTF-8")?;
    let (mut abi, normalized) = parse_abi_json(json)?;

    if normalized {
        eprintln!(
            "note: {} reports the upstream (pre-alkabi) ABI format; normalizing. \
             view/execute kinds are heuristic (get_* => view) — verify them.",
            abi.contract
        );
    }

    if plans {
        let config = AnalysisConfig {
            verify_trials: trials,
            ..AnalysisConfig::default()
        };
        eprintln!(
            "analyzing {} view methods for static plans ({} verification trials each)...",
            abi.methods
                .iter()
                .filter(|m| m.kind == alkabi::abi::MethodKind::View)
                .count(),
            trials,
        );
        attach_plans(&mut abi, &wasm, &config)?;
        let planned: Vec<&str> = abi
            .methods
            .iter()
            .filter(|m| m.plan.is_some())
            .map(|m| m.name.as_str())
            .collect();
        eprintln!(
            "synthesized {} verified plan(s): {}",
            planned.len(),
            if planned.is_empty() {
                "(none)".to_string()
            } else {
                planned.join(", ")
            },
        );
    }

    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("Failed to create {}", out_dir.display()))?;

    let json_path = out_dir.join("abi.json");
    std::fs::write(&json_path, format!("{}\n", abi.to_json_pretty()))?;

    let ts_path = out_dir.join("abi.ts");
    std::fs::write(&ts_path, abi.to_ts())?;

    println!("{}", json_path.display());
    println!("{}", ts_path.display());
    Ok(())
}
