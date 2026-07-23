//! Concolic WASM interpreter: runs a contract's `__execute` concretely (so the
//! allocator, borsh, formatting all execute for real) while propagating a
//! symbolic tag on values and memory bytes. Storage/height/calldata host loads
//! inject symbolic sources; the tags on the final `response.data` bytes become
//! the plan.
//!
//! Scope: the integer/memory/control subset alkanes views use. Anything
//! unsupported (floats, SIMD, an out-of-scope host call) bails — and because
//! the lifter's output is verified against the bytecode, a partial or wrong
//! lift is simply discarded.

use super::module::Module;
use super::sym::{ByteProv, SymBool, SymBytes, SymNum};
use anyhow::{anyhow, bail, Result};
use std::collections::BTreeMap;
use std::rc::Rc;
use wasmparser::{BlockType, Operator};

const PAGE: usize = 65536;

#[derive(Clone)]
struct Value {
    /// Concrete bits (i32 held in the low 32).
    c: u64,
    /// Symbolic numeric tag, if derived from storage/height/calldata.
    s: Option<Rc<SymNum>>,
    /// Symbolic boolean tag (comparison results), for select/branch capture.
    b: Option<Rc<SymBool>>,
}
impl Value {
    fn con(c: u64) -> Value {
        Value { c, s: None, b: None }
    }
    fn num(c: u64, s: Option<Rc<SymNum>>) -> Value {
        Value { c, s, b: None }
    }
    fn boolean(c: u64, b: Option<Rc<SymBool>>) -> Value {
        Value { c, s: None, b }
    }
    /// The value's symbolic number. A comparison result carries a boolean tag
    /// but no numeric tag; when it's consumed as a number (a multi-precision
    /// carry `(sum < a) as u64`, say) it must stay symbolic as `If(cond,1,0)`,
    /// not collapse to its concrete 0/1 — otherwise the surrounding arithmetic's
    /// structure would depend on the concrete input. Falls back to a constant
    /// only when the value is genuinely untagged.
    fn as_sym(&self) -> Rc<SymNum> {
        if let Some(s) = &self.s {
            return s.clone();
        }
        if let Some(b) = &self.b {
            return SymNum::If(b.clone(), SymNum::Const(1).rc(), SymNum::Const(0).rc()).rc();
        }
        SymNum::Const(self.c as u128).rc()
    }
    /// Whether the value carries any symbolic information (numeric or boolean).
    fn is_symbolic(&self) -> bool {
        self.s.is_some() || self.b.is_some()
    }
}

/// The storage oracle + context the interpreter runs against.
pub struct LiftEnv<'a> {
    pub context: Vec<u8>,
    /// byte offset in `context` where the post-opcode input words begin
    pub calldata_off: usize,
    pub height: u64,
    pub storage: &'a dyn Fn(&[u8]) -> Vec<u8>,
}

pub enum LiftOutcome {
    /// response.data as recovered symbolic bytes.
    Data(Rc<SymBytes>),
    Trap(String),
    Disqualified(&'static str),
    Unsupported(String),
}

pub struct Interp<'a> {
    m: &'a Module,
    mem: Vec<u8>,
    /// sparse symbolic provenance per memory byte
    tags: BTreeMap<u32, ByteProv>,
    globals: Vec<i64>,
    env: &'a LiftEnv<'a>,
    steps: u64,
    max_steps: u64,
    /// key SymBytes remembered between __request_storage and __load_storage
    pending_key: Option<Rc<SymBytes>>,
    disq: Option<&'static str>,
    /// Symbolic control-flow branches taken this run, in order — one entry per
    /// `if`/`br_if` whose condition carried a symbolic bool, holding the
    /// predicate and the direction taken. The predicate sequence is the path the
    /// merge / path-exploration driver reasons about.
    path: Vec<(Rc<SymBool>, bool)>,
    /// Forced directions for symbolic branches, by index. When the k-th symbolic
    /// branch is reached and `k < forced.len()`, the run takes `forced[k]`
    /// instead of the concrete direction — this is how [`explore`] walks both
    /// sides of a branch by deterministic re-execution (concolic path forking).
    /// Branches past the forced prefix follow their concrete value (and the
    /// first such is the exploration frontier).
    forced: Vec<bool>,
}

/// Cap on recorded branch predicates — guards against a symbolic byte-validation
/// loop flooding the trace; real view divergences sit well under this.
const MAX_PATH: usize = 64;

/// One entry per structural control block, for branch resolution.
struct Ctrl {
    /// operator index just past the matching `end`
    end: usize,
    /// for `loop`, the index to jump back to on branch; else same as `end`
    cont: usize,
    /// number of result values the label carries
    arity: usize,
    /// value-stack height at block entry
    height: usize,
    is_loop: bool,
}

impl<'a> Interp<'a> {
    pub fn new(m: &'a Module, env: &'a LiftEnv<'a>, max_steps: u64) -> Interp<'a> {
        let mut mem = m.memory.clone();
        if mem.len() < m.mem_min_pages as usize * PAGE {
            mem.resize(m.mem_min_pages as usize * PAGE, 0);
        }
        Interp {
            m,
            mem,
            tags: BTreeMap::new(),
            globals: m.globals.iter().map(|g| g.init).collect(),
            env,
            steps: 0,
            max_steps,
            pending_key: None,
            disq: None,
            path: Vec::new(),
            forced: Vec::new(),
        }
    }

    /// Force the first `forced.len()` symbolic branches to the given directions
    /// (for path exploration). Must be set before `run_execute`.
    pub fn set_forced(&mut self, forced: Vec<bool>) {
        self.forced = forced;
    }

    /// The branch predicates taken this run (see [`Interp::path`]).
    pub fn take_path(&mut self) -> Vec<(Rc<SymBool>, bool)> {
        std::mem::take(&mut self.path)
    }

    /// Resolve a branch's direction. Concrete conditions follow their value and
    /// aren't recorded (world-invariant). A symbolic condition is recorded, and
    /// its direction is overridden by `forced` when within the forced prefix —
    /// this is what lets [`explore`] re-execute down the other side of a branch.
    ///
    /// A condition is symbolic if it carries a boolean tag OR a numeric one: a
    /// u128 comparison is lowered by the compiler to a word-select yielding a
    /// 0/1 *number* (not a bool), so `br_if`-ing on it must still count as a
    /// symbolic branch (`value != 0`), else the branch it guards is invisible.
    fn decide(&mut self, cond: &Value, concrete: bool) -> bool {
        let pred: Option<Rc<SymBool>> = match (&cond.b, &cond.s) {
            (Some(b), _) => Some(b.clone()),
            (None, Some(s)) => Some(SymBool::Ne(s.clone(), SymNum::Const(0).rc()).rc()),
            (None, None) => None,
        };
        let Some(pred) = pred else {
            return concrete;
        };
        let k = self.path.len();
        let dir = self.forced.get(k).copied().unwrap_or(concrete);
        if k < MAX_PATH {
            self.path.push((pred, dir));
        }
        dir
    }

    pub fn run_execute(&mut self) -> LiftOutcome {
        let Some(&idx) = self.m.exports.get("__execute") else {
            return LiftOutcome::Unsupported("no __execute export".into());
        };
        match self.call(idx, vec![]) {
            Ok(results) => {
                if let Some(d) = self.disq {
                    return LiftOutcome::Disqualified(d);
                }
                let ptr = results.first().map(|v| v.c as u32).unwrap_or(0);
                self.extract_response(ptr)
            }
            Err(e) => {
                if let Some(d) = self.disq {
                    LiftOutcome::Disqualified(d)
                } else if e.to_string().starts_with("unsupported") {
                    LiftOutcome::Unsupported(e.to_string())
                } else {
                    LiftOutcome::Trap(e.to_string())
                }
            }
        }
    }

    fn tag_bytes(&mut self, addr: u32, src: &Rc<SymBytes>, len: u32) {
        for j in 0..len {
            self.tags.insert(
                addr + j,
                ByteProv {
                    src: src.clone(),
                    index: j,
                },
            );
        }
    }
    fn clear_tags(&mut self, addr: u32, len: u32) {
        for j in 0..len {
            self.tags.remove(&(addr + j));
        }
    }

    /* ─────────────── host imports ─────────────── */

    fn host_call(&mut self, name: &str, args: &[Value]) -> Result<Vec<Value>> {
        match name {
            "__request_context" => Ok(vec![Value::con(self.env.context.len() as u64)]),
            "__load_context" => {
                let ptr = args[0].c as u32;
                let ctx = self.env.context.clone();
                self.write_mem(ptr, &ctx)?;
                // tag the post-opcode input words as calldata bytes
                let off = self.env.calldata_off as u32;
                if (off as usize) < ctx.len() {
                    let cd_len = ctx.len() as u32 - off;
                    for j in 0..cd_len {
                        self.tags.insert(
                            ptr + off + j,
                            ByteProv {
                                src: SymBytes::Calldata {
                                    start: 0,
                                    len: None,
                                }
                                .rc(),
                                index: j,
                            },
                        );
                    }
                }
                Ok(vec![Value::con(ctx.len() as u64)])
            }
            "__request_storage" => {
                let key = self.read_key(args[0].c as u32)?;
                let concrete_key = self.concrete_bytes(args[0].c as u32);
                let val = (self.env.storage)(&concrete_key);
                self.pending_key = Some(key);
                Ok(vec![Value::con(val.len() as u64)])
            }
            "__load_storage" => {
                let key_ptr = args[0].c as u32;
                let out_ptr = args[1].c as u32;
                let concrete_key = self.concrete_bytes(key_ptr);
                let key_sym = self
                    .pending_key
                    .clone()
                    .unwrap_or_else(|| self.read_key(key_ptr).unwrap_or(SymBytes::Const(concrete_key.clone()).rc()));
                let val = (self.env.storage)(&concrete_key);
                self.write_mem(out_ptr, &val)?;
                let src = SymBytes::Storage(key_sym).rc();
                self.tag_bytes(out_ptr, &src, val.len() as u32);
                Ok(vec![Value::con(val.len() as u64)])
            }
            "__height" => {
                let ptr = args[0].c as u32;
                let h = self.env.height;
                self.write_mem(ptr, &h.to_le_bytes())?;
                let src = SymBytes::Le {
                    of: SymNum::Height.rc(),
                    width: 8,
                }
                .rc();
                self.tag_bytes(ptr, &src, 8);
                Ok(vec![])
            }
            // pure-but-unmodeled and impure host calls disqualify a plan
            "__call" | "__staticcall" | "__delegatecall" => {
                self.disq = Some("extcall");
                bail!("disqualified: extcall");
            }
            "__request_transaction" | "__load_transaction" => {
                self.disq = Some("transaction");
                bail!("disqualified: transaction");
            }
            "__request_block" | "__load_block" => {
                self.disq = Some("block");
                bail!("disqualified: block");
            }
            "__balance" => {
                self.disq = Some("__balance");
                bail!("disqualified: balance");
            }
            "__sequence" => {
                self.disq = Some("__sequence");
                bail!("disqualified: sequence");
            }
            "__returndatacopy" => {
                self.disq = Some("__returndatacopy");
                bail!("disqualified: returndatacopy");
            }
            "__log" => Ok(vec![]),
            "abort" => bail!("contract abort"),
            other => {
                self.disq = Some("unknown host import");
                bail!("disqualified: unknown host {}", other)
            }
        }
    }

    /// Read the arraybuffer at `ptr` (len is the u32 LE at ptr-4) concretely.
    fn concrete_bytes(&self, ptr: u32) -> Vec<u8> {
        if ptr < 4 || (ptr as usize) > self.mem.len() {
            return Vec::new();
        }
        let len =
            u32::from_le_bytes(self.mem[ptr as usize - 4..ptr as usize].try_into().unwrap()) as usize;
        let end = (ptr as usize + len).min(self.mem.len());
        self.mem[ptr as usize..end].to_vec()
    }

    /// Recover the key's symbolic bytes from the provenance at `ptr`.
    fn read_key(&self, ptr: u32) -> Result<Rc<SymBytes>> {
        let bytes = self.concrete_bytes(ptr);
        Ok(self.recover_bytes(ptr, bytes.len() as u32))
    }

    /// Assemble contiguous provenance runs at [addr, addr+len) into SymBytes.
    fn recover_bytes(&self, addr: u32, len: u32) -> Rc<SymBytes> {
        let mut parts: Vec<Rc<SymBytes>> = Vec::new();
        let mut i = 0u32;
        while i < len {
            let a = addr + i;
            match self.tags.get(&a) {
                Some(prov) => {
                    // extend a contiguous run of the same source with rising index
                    let start_index = prov.index;
                    let src = prov.src.clone();
                    let mut run = 1u32;
                    while i + run < len {
                        if let Some(next) = self.tags.get(&(addr + i + run)) {
                            if Rc::ptr_eq(&next.src, &src) && next.index == start_index + run {
                                run += 1;
                                continue;
                            }
                        }
                        break;
                    }
                    // a full run covering the whole source → use the source as-is
                    parts.push(if start_index == 0 {
                        src
                    } else {
                        // partial: fall back to a concrete slice of the memory
                        SymBytes::Const(
                            self.mem[a as usize..(a + run) as usize].to_vec(),
                        )
                        .rc()
                    });
                    i += run;
                }
                None => {
                    // concrete byte run
                    let start = i;
                    while i < len && self.tags.get(&(addr + i)).is_none() {
                        i += 1;
                    }
                    parts.push(
                        SymBytes::Const(
                            self.mem[(addr + start) as usize..(addr + i) as usize].to_vec(),
                        )
                        .rc(),
                    );
                }
            }
        }
        SymBytes::normalize(parts).rc()
    }

    /// Parse the serialized ExtendedCallResponse in memory at `ptr`, find the
    /// data byte range, and recover its symbolic bytes.
    fn extract_response(&self, ptr: u32) -> LiftOutcome {
        let bytes = self.concrete_bytes(ptr);
        // [count u128][ (block,tx,value) u128*3 ]*count [storage u32 pairs...] [data]
        let mut pos = 0usize;
        let rd_u128 = |b: &[u8], p: usize| -> Option<u128> {
            b.get(p..p + 16)
                .map(|s| u128::from_le_bytes(s.try_into().unwrap()))
        };
        let count = match rd_u128(&bytes, pos) {
            Some(c) if c <= 4096 => c as usize,
            _ => return LiftOutcome::Trap("bad response parcel".into()),
        };
        pos += 16 + count * 48;
        if pos + 4 > bytes.len() {
            return LiftOutcome::Trap("truncated response".into());
        }
        let pairs = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        for _ in 0..pairs {
            for _ in 0..2 {
                if pos + 4 > bytes.len() {
                    return LiftOutcome::Trap("truncated storage map".into());
                }
                let l = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
                pos += 4 + l;
            }
        }
        if pos > bytes.len() {
            return LiftOutcome::Trap("truncated after storage".into());
        }
        let data_len = bytes.len() - pos;
        let data = self.recover_bytes(ptr + pos as u32, data_len as u32);
        LiftOutcome::Data(data)
    }

    /* ─────────────── memory helpers ─────────────── */

    fn ensure(&mut self, addr: usize, len: usize) -> Result<()> {
        if addr + len > self.mem.len() {
            bail!("memory access out of bounds");
        }
        Ok(())
    }
    fn write_mem(&mut self, addr: u32, data: &[u8]) -> Result<()> {
        let a = addr as usize;
        self.ensure(a, data.len())?;
        self.mem[a..a + data.len()].copy_from_slice(data);
        self.clear_tags(addr, data.len() as u32);
        Ok(())
    }

    /* ─────────────── the interpreter core is in interp_exec.rs ─────────────── */

    fn call(&mut self, func_idx: u32, args: Vec<Value>) -> Result<Vec<Value>> {
        self.steps += 1;
        if self.steps > self.max_steps {
            bail!("step budget exceeded");
        }
        let n_imp = self.m.num_imported_funcs();
        if func_idx < n_imp {
            let imp = &self.m.imports[func_idx as usize];
            let name = imp.name.clone();
            return self.host_call(&name, &args);
        }
        let f = &self.m.funcs[(func_idx - n_imp) as usize];
        // locals = params (from args) + declared locals (zero-initialized)
        let mut locals: Vec<Value> = args;
        for _ in &f.locals {
            locals.push(Value::con(0));
        }
        self.exec_body(func_idx, locals)
    }
}

// The operator execution loop lives in a sibling module to keep files focused.
include!("interp_exec.rs");

/// Small helper used by both files.
fn block_arity(m: &Module, bt: &BlockType) -> usize {
    match bt {
        BlockType::Empty => 0,
        BlockType::Type(_) => 1,
        BlockType::FuncType(tidx) => m
            .types
            .get(*tidx as usize)
            .map(|s| s.results.len())
            .unwrap_or(0),
    }
}
fn block_params(m: &Module, bt: &BlockType) -> usize {
    match bt {
        BlockType::FuncType(tidx) => m
            .types
            .get(*tidx as usize)
            .map(|s| s.params.len())
            .unwrap_or(0),
        _ => 0,
    }
}
