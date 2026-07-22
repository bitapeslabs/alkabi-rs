//! The alkabi ABI document: what `export_abi()` serializes and `__meta` exposes.

use crate::plan::Plan;
use crate::schema::{write_json_string, Schema, TypeRegistry};

pub const ALKABI_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    /// Read-only; called through simulate.
    View,
    /// State-mutating; called through an on-chain transaction.
    Execute,
}

impl MethodKind {
    fn as_str(&self) -> &'static str {
        match self {
            MethodKind::View => "view",
            MethodKind::Execute => "execute",
        }
    }
}

/// How bytes on the wire relate to the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoMode {
    /// Positional u128 words after the opcode (u128 = 1 word, AlkaneId = 2 words,
    /// String = NUL-terminated LE-packed, Vec = length-prefixed).
    Legacy,
    /// borsh bytes chunked into 16-byte little-endian u128 words (inputs) or
    /// raw borsh bytes in `CallResponse.data` (outputs).
    Borsh,
    /// Raw response bytes: fixed-width integers as LE, strings as bare UTF-8 to
    /// end of buffer, `Vec<u8>` as the remaining bytes. Outputs only.
    Raw,
}

impl IoMode {
    fn as_str(&self) -> &'static str {
        match self {
            IoMode::Legacy => "legacy",
            IoMode::Borsh => "borsh",
            IoMode::Raw => "raw",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AbiIo {
    pub mode: IoMode,
    pub schema: Schema,
}

impl AbiIo {
    fn write_json(&self, out: &mut String) {
        out.push_str("{\"mode\":\"");
        out.push_str(self.mode.as_str());
        out.push_str("\",\"schema\":");
        self.schema.write_json(out);
        out.push('}');
    }
}

#[derive(Debug, Clone)]
pub struct AbiMethod {
    /// camelCase name, as the TS client will expose it.
    pub name: String,
    /// Written as a bare JSON number. JSON itself has no precision limit; only
    /// JS consumers reading opcodes above 2^53 with JSON.parse would need care.
    pub opcode: u128,
    pub kind: MethodKind,
    /// None means the method takes no calldata beyond the opcode.
    pub input: Option<AbiIo>,
    /// Borsh payload carried in the reveal transaction's witness envelope
    /// rather than in calldata. Mode is always Borsh.
    pub witness: Option<AbiIo>,
    /// None means void / no declared return.
    pub output: Option<AbiIo>,
    /// A verified pure-expression plan reproducing this view's response data
    /// from storage keys alone (no execution). Synthesized by the extractor's
    /// wasm analysis — never emitted by contracts themselves.
    pub plan: Option<Plan>,
}

impl AbiMethod {
    fn write_json(&self, out: &mut String) {
        out.push_str("{\"name\":");
        write_json_string(out, &self.name);
        out.push_str(",\"opcode\":");
        out.push_str(&self.opcode.to_string());
        out.push_str(",\"kind\":\"");
        out.push_str(self.kind.as_str());
        out.push('"');
        if let Some(input) = &self.input {
            out.push_str(",\"input\":");
            input.write_json(out);
        }
        if let Some(witness) = &self.witness {
            out.push_str(",\"witness\":");
            witness.write_json(out);
        }
        if let Some(output) = &self.output {
            out.push_str(",\"output\":");
            output.write_json(out);
        }
        if let Some(plan) = &self.plan {
            out.push_str(",\"plan\":");
            plan.write_json(out);
        }
        out.push('}');
    }
}

/// The alkabi ABI document. Contract-side, `export_abi()` builds one and
/// serializes it into `__meta`; host-side (feature `extract`), one is parsed
/// back out of any contract wasm. Re-exported at the crate root as `AlkabiAbi`.
#[derive(Debug)]
pub struct AbiDocument {
    pub contract: String,
    pub types: TypeRegistry,
    pub methods: Vec<AbiMethod>,
}

impl AbiDocument {
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\"alkabi\":");
        out.push_str(&ALKABI_VERSION.to_string());
        out.push_str(",\"contract\":");
        write_json_string(&mut out, &self.contract);
        out.push_str(",\"types\":");
        self.types.write_json(&mut out);
        out.push_str(",\"methods\":[");
        for (i, method) in self.methods.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            method.write_json(&mut out);
        }
        out.push_str("]}");
        out
    }

    /// Pretty-printed JSON (host-side convenience).
    #[cfg(feature = "extract")]
    pub fn to_json_pretty(&self) -> String {
        let value: serde_json::Value =
            serde_json::from_str(&self.to_json()).expect("alkabi writer emits valid JSON");
        serde_json::to_string_pretty(&value).expect("JSON value is serializable")
    }

    /// TypeScript module source: the document exported `as const` as
    /// `<Contract>Abi` — the contract name's casing is kept verbatim
    /// (`Tortilla` -> `TortillaAbi`, `AMMPool` -> `AMMPoolAbi`). The `as const`
    /// form is what gives a TS consumer literal types — a plain `.json` import
    /// widens them. Write this to an `abi.ts` file.
    #[cfg(feature = "extract")]
    pub fn to_ts(&self) -> String {
        let const_name = format!("{}Abi", self.contract);
        format!(
            "// Generated by alkabi — do not edit.\nexport const {} = {} as const;\n\nexport default {};\n",
            const_name,
            self.to_json_pretty(),
            const_name,
        )
    }
}
