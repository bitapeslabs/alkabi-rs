//! Thin CLI over `alkabi::extract`: reads a compiled contract wasm, extracts
//! its ABI (normalizing upstream-derive contracts), and writes `abi.json` +
//! `abi.ts` (an `as const` data file the TS side consumes for literal types).
//!
//! Usage: alkabi-extract <contract.wasm> [-o <out-dir>]   (out-dir defaults to ./abis)

use alkabi::extract::{extract_meta_bytes, parse_abi_json};
use anyhow::{bail, Context, Result};
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut wasm_path: Option<PathBuf> = None;
    let mut out_dir = PathBuf::from("abis");

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" | "--out" => {
                let value = args.next().context("-o requires a directory argument")?;
                out_dir = PathBuf::from(value);
            }
            "-h" | "--help" => {
                eprintln!("Usage: alkabi-extract <contract.wasm> [-o <out-dir>]");
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
    let (abi, normalized) = parse_abi_json(json)?;

    if normalized {
        eprintln!(
            "note: {} reports the upstream (pre-alkabi) ABI format; normalizing. \
             view/execute kinds are heuristic (get_* => view) — verify them.",
            abi.contract
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
