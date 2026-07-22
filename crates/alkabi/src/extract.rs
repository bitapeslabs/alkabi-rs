//! Host-side ABI extraction (feature `extract`): compiled contract wasm in,
//! [`AlkabiAbi`](crate::AlkabiAbi) out.
//!
//! ```no_run
//! let wasm = std::fs::read("contract.wasm")?;
//! let abi = alkabi::extract::extract_abi(&wasm)?;
//! println!("{}", abi.to_json_pretty());
//! # anyhow::Ok(())
//! ```
//!
//! Works on any alkanes contract with a `__meta` export: native alkabi
//! contracts pass through verbatim; contracts built with the upstream
//! `MessageDispatch` derive have their primitive ABI normalized into an alkabi
//! v1 document (snake_case -> camelCase, params -> legacy schemas, returns ->
//! raw schemas, and a `get_*` => view heuristic for `kind` since the upstream
//! enum never recorded it — verify those by hand).

use crate::abi::{AbiDocument, AbiIo, AbiMethod, IoMode, MethodKind};
use crate::schema::{Schema, TypeRegistry};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use wasmi::{Engine, Extern, ExternType, Global, Linker, Memory, Module, Store, Table, Val};

/// Instantiate the wasm, call `__meta`, parse, and normalize if needed.
pub fn extract_abi(wasm: &[u8]) -> Result<AbiDocument> {
    let bytes = extract_meta_bytes(wasm)?;
    let json = std::str::from_utf8(&bytes).context("__meta returned invalid UTF-8")?;
    Ok(parse_abi_json(json)?.0)
}

/// Parse an ABI JSON string (as returned by `__meta` or the indexer's `meta`
/// view) into a document. The second return value is true when the input was
/// the upstream (pre-alkabi) format and had to be normalized.
pub fn parse_abi_json(json: &str) -> Result<(AbiDocument, bool)> {
    let value: Value = serde_json::from_str(json).context("invalid ABI JSON")?;
    if value.get("alkabi").is_some() {
        Ok((parse_alkabi_document(&value)?, false))
    } else {
        Ok((normalize_upstream(&value)?, true))
    }
}

/// Run the contract's `__meta` export with every import stubbed and return the
/// raw bytes it exposes (the length-prefixed arraybuffer convention: u32 LE
/// length at ptr-4). The `__meta` path is pure — it only formats a string —
/// so no-op host stubs are safe.
pub fn extract_meta_bytes(wasm: &[u8]) -> Result<Vec<u8>> {
    let engine = Engine::default();
    let module = Module::new(&engine, wasm).map_err(|e| anyhow!("Invalid wasm: {}", e))?;
    let mut store = Store::new(&engine, ());
    let mut linker = <Linker<()>>::new(&engine);

    for import in module.imports() {
        let module_name = import.module();
        let field_name = import.name();
        match import.ty() {
            ExternType::Func(func_ty) => {
                let func_ty = func_ty.clone();
                let results: Vec<Val> = func_ty.results().iter().map(default_val).collect();
                linker
                    .func_new(
                        module_name,
                        field_name,
                        func_ty,
                        move |_caller, _params, outputs| {
                            for (slot, value) in outputs.iter_mut().zip(results.iter()) {
                                *slot = value.clone();
                            }
                            Ok(())
                        },
                    )
                    .map_err(|e| {
                        anyhow!("Failed to stub import {}::{}: {}", module_name, field_name, e)
                    })?;
            }
            ExternType::Memory(mem_ty) => {
                let memory = Memory::new(&mut store, *mem_ty)
                    .map_err(|e| anyhow!("Failed to create imported memory: {}", e))?;
                linker
                    .define(module_name, field_name, Extern::Memory(memory))
                    .map_err(|e| anyhow!("Failed to define imported memory: {}", e))?;
            }
            ExternType::Global(global_ty) => {
                let value = default_val(&global_ty.content());
                let global = Global::new(&mut store, value, global_ty.mutability());
                linker
                    .define(module_name, field_name, Extern::Global(global))
                    .map_err(|e| anyhow!("Failed to define imported global: {}", e))?;
            }
            ExternType::Table(table_ty) => {
                let init = default_val(&table_ty.element());
                let table = Table::new(&mut store, *table_ty, init)
                    .map_err(|e| anyhow!("Failed to create imported table: {}", e))?;
                linker
                    .define(module_name, field_name, Extern::Table(table))
                    .map_err(|e| anyhow!("Failed to define imported table: {}", e))?;
            }
        }
    }

    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| anyhow!("Failed to instantiate wasm: {}", e))?
        .start(&mut store)
        .map_err(|e| anyhow!("Failed to run wasm start: {}", e))?;

    let meta = instance
        .get_typed_func::<(), i32>(&store, "__meta")
        .map_err(|_| {
            anyhow!("Contract exports no __meta() -> i32 (is it built with alkabi or a MessageDispatch derive?)")
        })?;

    let ptr = meta
        .call(&mut store, ())
        .map_err(|e| anyhow!("__meta trapped: {}", e))? as u32 as usize;

    let memory = instance
        .get_memory(&store, "memory")
        .context("Contract exports no memory")?;
    let data = memory.data(&store);

    if ptr < 4 || ptr > data.len() {
        bail!("__meta returned an out-of-bounds pointer: {}", ptr);
    }
    let len = u32::from_le_bytes(data[ptr - 4..ptr].try_into().unwrap()) as usize;
    if ptr + len > data.len() {
        bail!("__meta length prefix out of bounds: ptr={} len={}", ptr, len);
    }
    Ok(data[ptr..ptr + len].to_vec())
}

fn default_val(ty: &wasmi::core::ValType) -> Val {
    use wasmi::core::ValType;
    match ty {
        ValType::I32 => Val::I32(0),
        ValType::I64 => Val::I64(0),
        ValType::F32 => Val::F32(0f32.into()),
        ValType::F64 => Val::F64(0f64.into()),
        ValType::FuncRef => Val::FuncRef(wasmi::FuncRef::null()),
        ValType::ExternRef => Val::ExternRef(wasmi::ExternRef::null()),
    }
}

// ---------------------------------------------------------------------------
// Parsing the alkabi v1 format
// ---------------------------------------------------------------------------

fn parse_alkabi_document(value: &Value) -> Result<AbiDocument> {
    let contract = value
        .get("contract")
        .and_then(Value::as_str)
        .context("ABI document has no \"contract\" field")?
        .to_string();

    let mut types = TypeRegistry::new();
    if let Some(map) = value.get("types").and_then(Value::as_object) {
        for (name, schema) in map {
            types.insert(name, parse_schema(schema).with_context(|| format!("type {}", name))?);
        }
    }

    let mut methods = Vec::new();
    for method in value
        .get("methods")
        .and_then(Value::as_array)
        .context("ABI document has no \"methods\" array")?
    {
        methods.push(parse_method(method)?);
    }

    Ok(AbiDocument {
        contract,
        types,
        methods,
    })
}

fn parse_method(value: &Value) -> Result<AbiMethod> {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .context("method missing \"name\"")?
        .to_string();
    let opcode = parse_opcode(value.get("opcode").context("method missing \"opcode\"")?)
        .with_context(|| format!("method {}", name))?;
    let kind = match value.get("kind").and_then(Value::as_str) {
        Some("view") => MethodKind::View,
        Some("execute") => MethodKind::Execute,
        other => bail!("method {}: unknown kind {:?}", name, other),
    };

    let parse_io = |key: &str| -> Result<Option<AbiIo>> {
        match value.get(key) {
            None => Ok(None),
            Some(io) => {
                let mode = match io.get("mode").and_then(Value::as_str) {
                    Some("legacy") => IoMode::Legacy,
                    Some("borsh") => IoMode::Borsh,
                    Some("raw") => IoMode::Raw,
                    other => bail!("method {}: unknown {} mode {:?}", name, key, other),
                };
                let schema = parse_schema(io.get("schema").context("io missing \"schema\"")?)
                    .with_context(|| format!("method {} {}", name, key))?;
                Ok(Some(AbiIo { mode, schema }))
            }
        }
    };

    let plan = match value.get("plan") {
        None => None,
        Some(plan) => Some(
            crate::plan::parse_plan(plan).with_context(|| format!("method {} plan", name))?,
        ),
    };

    Ok(AbiMethod {
        input: parse_io("input")?,
        witness: parse_io("witness")?,
        output: parse_io("output")?,
        plan,
        name,
        opcode,
        kind,
    })
}

fn parse_opcode(value: &Value) -> Result<u128> {
    match value {
        Value::Number(n) => n
            .to_string()
            .parse()
            .map_err(|_| anyhow!("opcode {} is not a u128", n)),
        Value::String(s) => s.parse().map_err(|_| anyhow!("opcode {:?} is not a u128", s)),
        other => bail!("opcode has unexpected type: {}", other),
    }
}

fn parse_schema(value: &Value) -> Result<Schema> {
    match value {
        Value::String(name) => {
            Schema::primitive(name).ok_or_else(|| anyhow!("unknown primitive {:?}", name))
        }
        Value::Object(map) => {
            if let Some(name) = map.get("$ref") {
                let name = name.as_str().context("$ref must be a string")?;
                Ok(Schema::Ref(name.to_string()))
            } else if let Some(fields) = map.get("struct") {
                let fields = fields.as_object().context("struct must be an object")?;
                let mut out = Vec::with_capacity(fields.len());
                for (fname, fschema) in fields {
                    out.push((fname.clone(), parse_schema(fschema)?));
                }
                Ok(Schema::Struct(out))
            } else if let Some(variants) = map.get("enum") {
                let variants = variants.as_array().context("enum must be an array")?;
                let mut out = Vec::with_capacity(variants.len());
                for variant in variants {
                    let wrapper = variant
                        .get("struct")
                        .and_then(Value::as_object)
                        .context("enum variant must be a {\"struct\": {Name: schema}} object")?;
                    if wrapper.len() != 1 {
                        bail!("enum variant wrapper must have exactly one key");
                    }
                    let (vname, vschema) = wrapper.iter().next().unwrap();
                    out.push((vname.clone(), parse_schema(vschema)?));
                }
                Ok(Schema::Enum(out))
            } else if let Some(inner) = map.get("option") {
                Ok(Schema::Option(Box::new(parse_schema(inner)?)))
            } else if let Some(array) = map.get("array") {
                let inner =
                    parse_schema(array.get("type").context("array missing \"type\"")?)?;
                match array.get("len") {
                    Some(len) => {
                        let len = len
                            .as_u64()
                            .context("array \"len\" must be an unsigned integer")?;
                        Ok(Schema::Array(Box::new(inner), len as usize))
                    }
                    None => Ok(Schema::Vec(Box::new(inner))),
                }
            } else {
                bail!("unrecognized schema object: {}", value)
            }
        }
        other => bail!("unrecognized schema value: {}", other),
    }
}

// ---------------------------------------------------------------------------
// Normalizing the upstream (pre-alkabi) format
// ---------------------------------------------------------------------------

fn normalize_upstream(value: &Value) -> Result<AbiDocument> {
    let contract = value
        .get("contract")
        .and_then(Value::as_str)
        .context("Unrecognized ABI document: no \"alkabi\" and no \"contract\" field")?
        .to_string();
    let methods_json = value
        .get("methods")
        .and_then(Value::as_array)
        .context("Unrecognized ABI document: no \"methods\" array")?;

    let mut types = TypeRegistry::new();
    let mut methods = Vec::new();

    for method in methods_json {
        let snake = method
            .get("name")
            .and_then(Value::as_str)
            .context("method missing \"name\"")?;
        let opcode = parse_opcode(method.get("opcode").context("method missing \"opcode\"")?)?;
        let kind = if snake == "get" || snake.starts_with("get_") {
            MethodKind::View
        } else {
            MethodKind::Execute
        };

        let input = match method.get("params").and_then(Value::as_array) {
            Some(params) if !params.is_empty() => {
                let mut fields = Vec::with_capacity(params.len());
                for param in params {
                    let pname = param
                        .get("name")
                        .and_then(Value::as_str)
                        .context("param missing \"name\"")?;
                    let pty = param
                        .get("type")
                        .and_then(Value::as_str)
                        .context("param missing \"type\"")?;
                    fields.push((pname.to_string(), upstream_type_schema(pty, &mut types)));
                }
                Some(AbiIo {
                    mode: IoMode::Legacy,
                    schema: Schema::Struct(fields),
                })
            }
            _ => None,
        };

        let output = match method.get("returns").and_then(Value::as_str) {
            None | Some("void") | Some("") => None,
            Some(returns) => {
                let schema = if returns.contains(',') {
                    let fields = returns
                        .split(',')
                        .enumerate()
                        .map(|(i, part)| {
                            (format!("_{}", i), upstream_type_schema(part.trim(), &mut types))
                        })
                        .collect();
                    Schema::Struct(fields)
                } else {
                    upstream_type_schema(returns.trim(), &mut types)
                };
                Some(AbiIo {
                    mode: IoMode::Raw,
                    schema,
                })
            }
        };

        methods.push(AbiMethod {
            name: snake_to_camel(snake),
            opcode,
            kind,
            input,
            witness: None,
            output,
            plan: None,
        });
    }

    Ok(AbiDocument {
        contract,
        types,
        methods,
    })
}

/// Map an upstream type string onto the alkabi schema grammar. Unknown names
/// become dangling `$ref`s (self-evident in the output).
fn upstream_type_schema(ty: &str, types: &mut TypeRegistry) -> Schema {
    if let Some(primitive) = Schema::primitive(ty) {
        return primitive;
    }
    match ty {
        "String" => Schema::Primitive("string"),
        "AlkaneId" => {
            if !types.contains("AlkaneId") {
                types.insert(
                    "AlkaneId",
                    Schema::Struct(vec![
                        ("block".to_string(), Schema::Primitive("u128")),
                        ("tx".to_string(), Schema::Primitive("u128")),
                    ]),
                );
            }
            Schema::Ref("AlkaneId".to_string())
        }
        _ => {
            if let Some(inner) = ty.strip_prefix("Vec<").and_then(|s| s.strip_suffix('>')) {
                Schema::Vec(Box::new(upstream_type_schema(inner.trim(), types)))
            } else {
                Schema::Ref(ty.to_string())
            }
        }
    }
}

fn snake_to_camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut upper_next = false;
    for c in s.chars() {
        if c == '_' {
            upper_next = true;
        } else if upper_next {
            out.extend(c.to_uppercase());
            upper_next = false;
        } else {
            out.push(c);
        }
    }
    out
}
