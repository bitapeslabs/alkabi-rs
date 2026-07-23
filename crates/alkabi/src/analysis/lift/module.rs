//! Minimal WASM module representation for the concolic lifter — enough to
//! interpret an alkanes contract's `__execute`: function bodies as operator
//! vectors, types, initialized linear memory, globals, imports/exports, and
//! the table (for `call_indirect`).

use anyhow::{anyhow, bail, Result};
use wasmparser::{
    Chunk, DataKind, ElementItems, ElementKind, FuncType, Operator, Parser, Payload, TableInit,
    TypeRef, ValType,
};

#[derive(Debug, Clone)]
pub struct FuncSig {
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
}

impl From<&FuncType> for FuncSig {
    fn from(f: &FuncType) -> Self {
        FuncSig {
            params: f.params().to_vec(),
            results: f.results().to_vec(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Function {
    pub sig: FuncSig,
    /// Local declarations (already flattened): types of locals after params.
    pub locals: Vec<ValType>,
    /// The function body as a flat operator vector (owned).
    pub body: Vec<Operator<'static>>,
}

#[derive(Debug, Clone)]
pub struct ImportedFunc {
    pub module: String,
    pub name: String,
    pub sig: FuncSig,
}

#[derive(Debug, Clone, Copy)]
pub struct GlobalDef {
    pub ty: ValType,
    pub mutable: bool,
    pub init: i64,
}

pub struct Module {
    /// Owns the wasm bytes the operator bodies borrow from (kept alive for the
    /// module's lifetime; never mutated after construction).
    _wasm: Vec<u8>,
    pub types: Vec<FuncSig>,
    /// Imported functions come first in the function index space.
    pub imports: Vec<ImportedFunc>,
    /// Local (defined) functions, indexed after imports.
    pub funcs: Vec<Function>,
    pub globals: Vec<GlobalDef>,
    /// Initial linear memory (min pages worth) with data segments applied.
    pub memory: Vec<u8>,
    pub mem_min_pages: u32,
    pub mem_max_pages: Option<u32>,
    /// Table of function indices for call_indirect (funcref table 0).
    pub table: Vec<Option<u32>>,
    /// exported name -> function index.
    pub exports: std::collections::HashMap<String, u32>,
}

impl Module {
    pub fn num_imported_funcs(&self) -> u32 {
        self.imports.len() as u32
    }

    /// Function index -> signature (imported or local).
    pub fn func_sig(&self, idx: u32) -> Option<&FuncSig> {
        let n = self.imports.len() as u32;
        if idx < n {
            Some(&self.imports[idx as usize].sig)
        } else {
            self.funcs.get((idx - n) as usize).map(|f| &f.sig)
        }
    }

    pub fn parse(wasm_in: &[u8]) -> Result<Module> {
        let owned: Vec<u8> = wasm_in.to_vec();
        // SAFETY: the operator bodies produced below borrow from `owned`, which
        // is moved into the returned Module and never mutated afterward. A Vec's
        // heap buffer address is stable across the move, so the 'static borrow
        // stays valid for the Module's lifetime.
        let wasm: &'static [u8] =
            unsafe { std::mem::transmute::<&[u8], &'static [u8]>(owned.as_slice()) };

        let mut types: Vec<FuncSig> = Vec::new();
        let mut imports: Vec<ImportedFunc> = Vec::new();
        let mut func_type_idx: Vec<u32> = Vec::new(); // local functions' type indices
        let mut funcs: Vec<Function> = Vec::new();
        let mut globals: Vec<GlobalDef> = Vec::new();
        let mut exports = std::collections::HashMap::new();
        let mut mem_min_pages = 0u32;
        let mut mem_max_pages = None;
        let mut memory: Vec<u8> = Vec::new();
        let mut table: Vec<Option<u32>> = Vec::new();

        let mut parser = Parser::new(0);
        let mut stack: Vec<u8> = Vec::new();
        let _ = &mut stack;

        let mut remaining = wasm;
        loop {
            let (payload, consumed) = match parser.parse(remaining, true)? {
                Chunk::NeedMoreData(_) => bail!("wasm: unexpected end of input"),
                Chunk::Parsed { payload, consumed } => (payload, consumed),
            };
            match payload {
                Payload::TypeSection(reader) => {
                    for rec_group in reader {
                        let rg = rec_group?;
                        for sub in rg.types() {
                            if let wasmparser::CompositeInnerType::Func(f) =
                                &sub.composite_type.inner
                            {
                                types.push(FuncSig::from(f));
                            } else {
                                types.push(FuncSig {
                                    params: vec![],
                                    results: vec![],
                                });
                            }
                        }
                    }
                }
                Payload::ImportSection(reader) => {
                    for import in reader {
                        let import = import?;
                        if let TypeRef::Func(tidx) = import.ty {
                            let sig = types
                                .get(tidx as usize)
                                .ok_or_else(|| anyhow!("import: bad type idx"))?
                                .clone();
                            imports.push(ImportedFunc {
                                module: import.module.to_string(),
                                name: import.name.to_string(),
                                sig,
                            });
                        }
                        // imported memory/global/table: alkanes contracts define
                        // their own; ignore imported ones (bail later if used).
                    }
                }
                Payload::FunctionSection(reader) => {
                    for tidx in reader {
                        func_type_idx.push(tidx?);
                    }
                }
                Payload::TableSection(reader) => {
                    for t in reader {
                        let t = t?;
                        let init_len = t.ty.initial as usize;
                        table = vec![None; init_len];
                        if let TableInit::Expr(_) = &t.init {
                            // ref.func init expr — rare for these contracts; skip.
                        }
                    }
                }
                Payload::MemorySection(reader) => {
                    for m in reader {
                        let m = m?;
                        mem_min_pages = m.initial as u32;
                        mem_max_pages = m.maximum.map(|x| x as u32);
                        memory = vec![0u8; (mem_min_pages as usize) * 65536];
                    }
                }
                Payload::GlobalSection(reader) => {
                    for g in reader {
                        let g = g?;
                        let init = const_expr_i64(&g.init_expr)?;
                        globals.push(GlobalDef {
                            ty: g.ty.content_type,
                            mutable: g.ty.mutable,
                            init,
                        });
                    }
                }
                Payload::ExportSection(reader) => {
                    for e in reader {
                        let e = e?;
                        if let wasmparser::ExternalKind::Func = e.kind {
                            exports.insert(e.name.to_string(), e.index);
                        }
                    }
                }
                Payload::ElementSection(reader) => {
                    for el in reader {
                        let el = el?;
                        if let ElementKind::Active {
                            offset_expr,
                            table_index,
                        } = el.kind
                        {
                            if table_index.unwrap_or(0) != 0 {
                                continue;
                            }
                            let base = const_expr_i64(&offset_expr)? as usize;
                            let mut funcs_in_seg: Vec<u32> = Vec::new();
                            match el.items {
                                ElementItems::Functions(fs) => {
                                    for f in fs {
                                        funcs_in_seg.push(f?);
                                    }
                                }
                                ElementItems::Expressions(_, exprs) => {
                                    for e in exprs {
                                        // ref.func const expr
                                        if let Some(fidx) = const_expr_ref_func(&e?) {
                                            funcs_in_seg.push(fidx);
                                        } else {
                                            funcs_in_seg.push(u32::MAX);
                                        }
                                    }
                                }
                            }
                            if base + funcs_in_seg.len() > table.len() {
                                table.resize(base + funcs_in_seg.len(), None);
                            }
                            for (i, f) in funcs_in_seg.into_iter().enumerate() {
                                table[base + i] = if f == u32::MAX { None } else { Some(f) };
                            }
                        }
                    }
                }
                Payload::DataSection(reader) => {
                    for d in reader {
                        let d = d?;
                        if let DataKind::Active {
                            offset_expr,
                            memory_index: _,
                        } = d.kind
                        {
                            let base = const_expr_i64(&offset_expr)? as usize;
                            if base + d.data.len() > memory.len() {
                                memory.resize(base + d.data.len(), 0);
                            }
                            memory[base..base + d.data.len()].copy_from_slice(d.data);
                        }
                    }
                }
                Payload::CodeSectionEntry(body) => {
                    let idx = funcs.len();
                    let tidx = *func_type_idx
                        .get(idx)
                        .ok_or_else(|| anyhow!("code: missing type idx"))?;
                    let sig = types
                        .get(tidx as usize)
                        .ok_or_else(|| anyhow!("code: bad type idx"))?
                        .clone();

                    let mut locals = Vec::new();
                    for local in body.get_locals_reader()? {
                        let (count, ty) = local?;
                        for _ in 0..count {
                            locals.push(ty);
                        }
                    }
                    let mut ops = Vec::new();
                    for op in body.get_operators_reader()? {
                        ops.push(op?.clone());
                    }
                    funcs.push(Function { sig, locals, body: ops });
                }
                Payload::End(_) => break,
                _ => {}
            }
            remaining = &remaining[consumed..];
        }

        if memory.is_empty() {
            memory = vec![0u8; 65536];
            mem_min_pages = mem_min_pages.max(1);
        }

        Ok(Module {
            _wasm: owned,
            types,
            imports,
            funcs,
            globals,
            memory,
            mem_min_pages,
            mem_max_pages,
            table,
            exports,
        })
    }
}

fn const_expr_i64(expr: &wasmparser::ConstExpr) -> Result<i64> {
    let mut reader = expr.get_operators_reader();
    let op = reader.read()?;
    let v = match op {
        Operator::I32Const { value } => value as i64,
        Operator::I64Const { value } => value,
        Operator::GlobalGet { .. } => 0, // imported-global base; treat as 0
        _ => bail!("unsupported const expr"),
    };
    Ok(v)
}

fn const_expr_ref_func(expr: &wasmparser::ConstExpr) -> Option<u32> {
    let mut reader = expr.get_operators_reader();
    match reader.read().ok()? {
        Operator::RefFunc { function_index } => Some(function_index),
        _ => None,
    }
}
