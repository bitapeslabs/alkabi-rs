//! The probe host: a minimal alkanes indexer-in-a-box that runs a contract's
//! `__execute` under full oracle control — we supply the context (opcode +
//! inputs), the storage values, and the chain height, and we record every
//! storage key the contract requests plus the response it produces.
//!
//! Conventions mirrored from the real host (alkanes-rs / metashrew):
//!   - passback pointers: data at `ptr`, u32 LE length at `ptr - 4`
//!   - `__request_storage(key) -> len` then `__load_storage(key, out)`
//!   - `__request_context() -> len` then `__load_context(out)`
//!   - `__height(out)` writes a u64 LE
//!   - context bytes: flat u128 LE words `[myself.block, myself.tx,
//!     caller.block, caller.tx, vout, transfer_count, (block, tx, value)*,
//!     opcode, inputs...]`
//!   - errors abort (trap); a successful `__execute` returns a pointer to a
//!     serialized ExtendedCallResponse (parcel, storage map, data)

use anyhow::{anyhow, bail, Result};
use std::collections::BTreeMap;
use wasmi::{Caller, Config, Engine, Extern, ExternType, Global, Linker, Memory, Module, Store, Table, Val};

/// The world a probe run executes against.
#[derive(Debug, Clone)]
pub struct Oracle {
    /// Missing keys read as zero-length values (matching the real host).
    pub storage: BTreeMap<Vec<u8>, Vec<u8>>,
    pub height: u64,
    pub myself: (u128, u128),
    pub caller: (u128, u128),
    pub vout: u128,
    pub incoming: Vec<(u128, u128, u128)>,
}

impl Default for Oracle {
    fn default() -> Self {
        Self {
            storage: BTreeMap::new(),
            height: 880_001,
            myself: (2, 7777),
            caller: (0, 0),
            vout: 4,
            incoming: Vec::new(),
        }
    }
}

impl Oracle {
    fn context_bytes(&self, opcode: u128, args: &[u128]) -> Vec<u8> {
        let mut words: Vec<u128> = vec![
            self.myself.0,
            self.myself.1,
            self.caller.0,
            self.caller.1,
            self.vout,
            self.incoming.len() as u128,
        ];
        for (block, tx, value) in &self.incoming {
            words.extend([*block, *tx, *value]);
        }
        words.push(opcode);
        words.extend_from_slice(args);
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }
}

#[derive(Debug, Clone)]
pub enum Outcome {
    /// `CallResponse.data` of a successful execution.
    Success(Vec<u8>),
    /// The contract aborted or trapped (alkanes errors always abort).
    Trap(String),
    /// The method used a host facility plans cannot model (extcalls,
    /// transaction/block access, balances, ...).
    Disqualified(&'static str),
}

#[derive(Debug, Clone)]
pub struct RunResult {
    pub outcome: Outcome,
    /// Distinct storage keys in first-request order.
    pub keys: Vec<Vec<u8>>,
}

struct ProbeState {
    oracle: Oracle,
    context: Vec<u8>,
    keys: Vec<Vec<u8>>,
    disqualified: Option<&'static str>,
}

fn read_passback(memory: &[u8], ptr: i32) -> Result<Vec<u8>> {
    let ptr = ptr as usize;
    if ptr < 4 || ptr > memory.len() {
        bail!("probe: passback pointer {} out of bounds", ptr);
    }
    let len = u32::from_le_bytes(memory[ptr - 4..ptr].try_into().unwrap()) as usize;
    if ptr + len > memory.len() {
        bail!("probe: passback length {} out of bounds", len);
    }
    Ok(memory[ptr..ptr + len].to_vec())
}

fn guest_memory(caller: &mut Caller<'_, ProbeState>) -> Result<Memory, wasmi::Error> {
    caller
        .get_export("memory")
        .and_then(Extern::into_memory)
        .ok_or_else(|| wasmi::Error::new("probe: contract exports no memory"))
}

fn host_err(e: anyhow::Error) -> wasmi::Error {
    wasmi::Error::new(e.to_string())
}

/// Parse an ExtendedCallResponse and return its `data` field.
fn response_data(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut pos = 0usize;
    let word = |pos: &mut usize| -> Result<u128> {
        if *pos + 16 > bytes.len() {
            bail!("probe: response truncated");
        }
        let w = u128::from_le_bytes(bytes[*pos..*pos + 16].try_into().unwrap());
        *pos += 16;
        Ok(w)
    };
    let count = word(&mut pos)?;
    if count > 4096 {
        bail!("probe: implausible transfer count {}", count);
    }
    pos += (count as usize) * 48; // (block, tx, value) per transfer
    if pos + 4 > bytes.len() {
        bail!("probe: response truncated at storage map");
    }
    let pairs = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;
    for _ in 0..pairs {
        for _ in 0..2 {
            if pos + 4 > bytes.len() {
                bail!("probe: response truncated in storage map");
            }
            let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4 + len;
        }
    }
    if pos > bytes.len() {
        bail!("probe: response truncated after storage map");
    }
    Ok(bytes[pos..].to_vec())
}

pub struct Prober {
    engine: Engine,
    module: Module,
    fuel: u64,
}

impl Prober {
    pub fn new(wasm: &[u8], fuel: u64) -> Result<Self> {
        let mut config = Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module =
            Module::new(&engine, wasm).map_err(|e| anyhow!("probe: invalid wasm: {}", e))?;
        Ok(Self {
            engine,
            module,
            fuel,
        })
    }

    /// Execute the view once (fresh instance) against the oracle.
    pub fn run(&self, opcode: u128, args: &[u128], oracle: &Oracle) -> Result<RunResult> {
        let state = ProbeState {
            context: oracle.context_bytes(opcode, args),
            oracle: oracle.clone(),
            keys: Vec::new(),
            disqualified: None,
        };
        let mut store = Store::new(&self.engine, state);
        store
            .set_fuel(self.fuel)
            .map_err(|e| anyhow!("probe: set_fuel: {}", e))?;

        let mut linker = <Linker<ProbeState>>::new(&self.engine);
        self.define_imports(&mut linker, &mut store)?;

        let instance = match linker
            .instantiate(&mut store, &self.module)
            .and_then(|pre| pre.start(&mut store))
        {
            Ok(instance) => instance,
            Err(e) => bail!("probe: instantiation failed: {}", e),
        };

        let execute = instance
            .get_typed_func::<(), i32>(&store, "__execute")
            .map_err(|_| anyhow!("probe: contract exports no __execute"))?;

        let call_result = execute.call(&mut store, ());
        let keys = {
            let mut seen = std::collections::BTreeSet::new();
            store
                .data()
                .keys
                .iter()
                .filter(|k| seen.insert((*k).clone()))
                .cloned()
                .collect::<Vec<_>>()
        };

        if let Some(import) = store.data().disqualified {
            return Ok(RunResult {
                outcome: Outcome::Disqualified(import),
                keys,
            });
        }

        let outcome = match call_result {
            Err(trap) => Outcome::Trap(trap.to_string()),
            Ok(ptr) => {
                let memory = instance
                    .get_memory(&store, "memory")
                    .ok_or_else(|| anyhow!("probe: contract exports no memory"))?;
                match read_passback(memory.data(&store), ptr) {
                    Ok(response) => match response_data(&response) {
                        Ok(data) => Outcome::Success(data),
                        Err(e) => Outcome::Trap(format!("bad response: {}", e)),
                    },
                    Err(e) => Outcome::Trap(format!("bad response pointer: {}", e)),
                }
            }
        };

        Ok(RunResult { outcome, keys })
    }

    /// Define every import by dispatching on its name against its ACTUAL
    /// declared signature (host signatures drift across metashrew versions, so
    /// we adapt to whatever the module asks for rather than hardcoding arities
    /// or return types). Non-func imports get defaults.
    fn define_imports(
        &self,
        linker: &mut Linker<ProbeState>,
        store: &mut Store<ProbeState>,
    ) -> Result<()> {
        for import in self.module.imports() {
            let module_name = import.module();
            let field_name = import.name().to_string();
            match import.ty() {
                ExternType::Func(func_ty) => {
                    let func_ty = func_ty.clone();
                    let name = field_name.clone();
                    let default_results: Vec<Val> =
                        func_ty.results().iter().map(default_val).collect();
                    let _ = linker.func_new(
                        module_name,
                        &field_name,
                        func_ty,
                        move |mut caller, params, outputs| {
                            host_dispatch(&name, &mut caller, params, outputs, &default_results)
                        },
                    );
                }
                ExternType::Memory(mem_ty) => {
                    let memory = Memory::new(&mut *store, *mem_ty)
                        .map_err(|e| anyhow!("probe: imported memory: {}", e))?;
                    let _ = linker.define(module_name, &field_name, Extern::Memory(memory));
                }
                ExternType::Global(global_ty) => {
                    let value = default_val(&global_ty.content());
                    let global = Global::new(&mut *store, value, global_ty.mutability());
                    let _ = linker.define(module_name, &field_name, Extern::Global(global));
                }
                ExternType::Table(table_ty) => {
                    let init = default_val(&table_ty.element());
                    let table = Table::new(&mut *store, *table_ty, init)
                        .map_err(|e| anyhow!("probe: imported table: {}", e))?;
                    let _ = linker.define(module_name, &field_name, Extern::Table(table));
                }
            }
        }
        Ok(())
    }
}

/// i32 argument at position `i`, or 0.
fn arg_i32(params: &[Val], i: usize) -> i32 {
    match params.get(i) {
        Some(Val::I32(v)) => *v,
        Some(Val::I64(v)) => *v as i32,
        _ => 0,
    }
}

/// Write `value` to every result slot that expects it (results are already the
/// declared defaults; we only overwrite when we have something to return).
fn set_result_i32(outputs: &mut [Val], value: i32) {
    if let Some(slot) = outputs.first_mut() {
        match slot {
            Val::I32(_) => *slot = Val::I32(value),
            Val::I64(_) => *slot = Val::I64(value as i64),
            _ => {}
        }
    }
}

/// The name-keyed host implementation, signature-agnostic: it reads i32 args
/// positionally and writes an i32 result when the declared signature has one.
fn host_dispatch(
    name: &str,
    caller: &mut Caller<'_, ProbeState>,
    params: &[Val],
    outputs: &mut [Val],
    default_results: &[Val],
) -> Result<(), wasmi::Error> {
    // start from declared-default results
    for (slot, def) in outputs.iter_mut().zip(default_results.iter()) {
        *slot = def.clone();
    }

    match name {
        "__request_context" => {
            let len = caller.data().context.len() as i32;
            set_result_i32(outputs, len);
        }
        "__load_context" => {
            let ptr = arg_i32(params, 0);
            let memory = guest_memory(caller)?;
            let (data, state) = memory.data_and_store_mut(caller);
            let ptr = ptr as usize;
            let ctx = &state.context;
            if ptr + ctx.len() > data.len() {
                return Err(wasmi::Error::new("probe: context buffer out of bounds"));
            }
            data[ptr..ptr + ctx.len()].copy_from_slice(ctx);
            set_result_i32(outputs, ctx.len() as i32);
        }
        "__request_storage" => {
            let key_ptr = arg_i32(params, 0);
            let memory = guest_memory(caller)?;
            let key = read_passback(memory.data(&*caller), key_ptr).map_err(host_err)?;
            let state = caller.data_mut();
            let len = state.oracle.storage.get(&key).map(|v| v.len()).unwrap_or(0);
            state.keys.push(key);
            set_result_i32(outputs, len as i32);
        }
        "__load_storage" => {
            let key_ptr = arg_i32(params, 0);
            let value_ptr = arg_i32(params, 1);
            let memory = guest_memory(caller)?;
            let key = read_passback(memory.data(&*caller), key_ptr).map_err(host_err)?;
            let value = caller
                .data()
                .oracle
                .storage
                .get(&key)
                .cloned()
                .unwrap_or_default();
            let data = memory.data_mut(caller);
            let ptr = value_ptr as usize;
            if ptr + value.len() > data.len() {
                return Err(wasmi::Error::new("probe: storage buffer out of bounds"));
            }
            data[ptr..ptr + value.len()].copy_from_slice(&value);
            set_result_i32(outputs, value.len() as i32);
        }
        "__height" => {
            let ptr = arg_i32(params, 0);
            let memory = guest_memory(caller)?;
            let height = caller.data().oracle.height;
            let data = memory.data_mut(caller);
            let ptr = ptr as usize;
            if ptr + 8 > data.len() {
                return Err(wasmi::Error::new("probe: height buffer out of bounds"));
            }
            data[ptr..ptr + 8].copy_from_slice(&height.to_le_bytes());
        }
        "__log" => {}
        "abort" => {
            return Err(wasmi::Error::new("contract abort"));
        }
        // Everything else — extcalls, tx/block access, balances, sequence,
        // fuel, returndata, and any unknown host func — is out of a plan's
        // reach; using it disqualifies the method (default results returned).
        _ => {
            caller
                .data_mut()
                .disqualified
                .get_or_insert(disqualifier_name(name));
        }
    }
    Ok(())
}

fn disqualifier_name(name: &str) -> &'static str {
    match name {
        "__call" => "__call",
        "__staticcall" => "__staticcall",
        "__delegatecall" => "__delegatecall",
        "__request_transaction" | "__load_transaction" => "transaction",
        "__request_block" | "__load_block" => "block",
        "__balance" => "__balance",
        "__sequence" => "__sequence",
        "__fuel" => "__fuel",
        "__returndatacopy" => "__returndatacopy",
        _ => "unknown import",
    }
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
