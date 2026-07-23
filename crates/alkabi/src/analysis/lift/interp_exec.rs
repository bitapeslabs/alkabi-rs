// Operator execution loop for the concolic interpreter (included into interp.rs
// as a continuation of `impl<'a> Interp<'a>`). Single concrete path; symbolic
// tags propagate through the integer/memory subset. Anything unsupported bails.

impl<'a> Interp<'a> {
    fn exec_body(&mut self, func_idx: u32, mut locals: Vec<Value>) -> Result<Vec<Value>> {
        let n_imp = self.m.num_imported_funcs();
        let f = &self.m.funcs[(func_idx - n_imp) as usize];
        let body: Vec<Operator<'static>> = f.body.clone();
        let n_results = f.sig.results.len();

        // Precompute matching else/end for each structured opener.
        let (ends, elses) = block_structure(&body)?;

        let mut vs: Vec<Value> = Vec::new();
        let mut cs: Vec<Ctrl> = Vec::new();
        let mut pc = 0usize;

        macro_rules! pop {
            () => {
                vs.pop().ok_or_else(|| anyhow!("value stack underflow"))?
            };
        }

        while pc < body.len() {
            self.steps += 1;
            if self.steps > self.max_steps {
                bail!("step budget exceeded");
            }
            match &body[pc] {
                Operator::Unreachable => bail!("unreachable"),
                Operator::Nop => {}
                Operator::Block { blockty } => {
                    let arity = block_arity(self.m, blockty);
                    if block_params(self.m, blockty) != 0 {
                        bail!("unsupported: multivalue block params");
                    }
                    cs.push(Ctrl {
                        end: ends[&pc],
                        cont: ends[&pc] + 1,
                        arity,
                        height: vs.len(),
                        is_loop: false,
                    });
                }
                Operator::Loop { blockty } => {
                    if block_params(self.m, blockty) != 0 {
                        bail!("unsupported: multivalue loop params");
                    }
                    cs.push(Ctrl {
                        end: ends[&pc],
                        cont: pc + 1, // branch to loop re-enters at body start
                        arity: 0,
                        height: vs.len(),
                        is_loop: true,
                    });
                }
                Operator::If { blockty } => {
                    let arity = block_arity(self.m, blockty);
                    if block_params(self.m, blockty) != 0 {
                        bail!("unsupported: multivalue if params");
                    }
                    let cond = pop!();
                    let taken = self.decide(&cond, cond.c & 0xffff_ffff != 0);
                    cs.push(Ctrl {
                        end: ends[&pc],
                        cont: ends[&pc] + 1,
                        arity,
                        height: vs.len(),
                        is_loop: false,
                    });
                    if !taken {
                        // false → jump to else body or past end
                        match elses.get(&pc) {
                            Some(&e) => pc = e, // land on Else; +1 below
                            None => {
                                cs.pop();
                                pc = ends[&pc];
                                continue;
                            }
                        }
                    }
                }
                Operator::Else => {
                    // reached at end of then-branch → skip else, go to matching end
                    let end = cs.last().map(|c| c.end).ok_or_else(|| anyhow!("else w/o if"))?;
                    pc = end;
                    continue;
                }
                Operator::End => {
                    if cs.pop().is_none() {
                        // function end
                        let mut out = Vec::new();
                        for _ in 0..n_results {
                            out.push(pop!());
                        }
                        out.reverse();
                        return Ok(out);
                    }
                }
                Operator::Br { relative_depth } => {
                    self.do_branch(*relative_depth as usize, &mut vs, &mut cs, &mut pc);
                    continue;
                }
                Operator::BrIf { relative_depth } => {
                    let c = pop!();
                    let taken = self.decide(&c, c.c & 0xffff_ffff != 0);
                    if taken {
                        self.do_branch(*relative_depth as usize, &mut vs, &mut cs, &mut pc);
                        continue;
                    }
                }
                Operator::BrTable { targets } => {
                    let idx = pop!().c as u32;
                    let depth = targets
                        .targets()
                        .nth(idx as usize)
                        .and_then(|r| r.ok())
                        .unwrap_or_else(|| targets.default());
                    self.do_branch(depth as usize, &mut vs, &mut cs, &mut pc);
                    continue;
                }
                Operator::Return => {
                    let mut out = Vec::new();
                    for _ in 0..n_results {
                        out.push(pop!());
                    }
                    out.reverse();
                    return Ok(out);
                }
                Operator::Call { function_index } => {
                    let sig = self
                        .m
                        .func_sig(*function_index)
                        .ok_or_else(|| anyhow!("bad call target"))?
                        .clone();
                    let mut args = Vec::with_capacity(sig.params.len());
                    for _ in 0..sig.params.len() {
                        args.push(pop!());
                    }
                    args.reverse();
                    let res = self.call(*function_index, args)?;
                    vs.extend(res);
                }
                Operator::CallIndirect { .. } => {
                    let idx = pop!().c as usize;
                    let target = self
                        .m
                        .table
                        .get(idx)
                        .copied()
                        .flatten()
                        .ok_or_else(|| anyhow!("call_indirect: null/oob"))?;
                    let sig = self
                        .m
                        .func_sig(target)
                        .ok_or_else(|| anyhow!("bad indirect target"))?
                        .clone();
                    let mut args = Vec::with_capacity(sig.params.len());
                    for _ in 0..sig.params.len() {
                        args.push(pop!());
                    }
                    args.reverse();
                    let res = self.call(target, args)?;
                    vs.extend(res);
                }
                Operator::Drop => {
                    pop!();
                }
                Operator::Select | Operator::TypedSelect { .. } => {
                    let c = pop!();
                    let bv = pop!();
                    let av = pop!();
                    let taken_c = if c.c & 0xffff_ffff != 0 { av.c } else { bv.c };
                    // capture branchless conditionals: if the selector is
                    // symbolic and either branch carries symbolic info, produce a
                    // conditional value instead of collapsing to one side. The
                    // selector counts as symbolic via its boolean tag OR a
                    // numeric one — a u128 saturating-sub selects on its *borrow*,
                    // which the word arithmetic yields as a 0/1 number, not a
                    // bool; missing that drops the saturation guard entirely.
                    let sel: Option<Rc<SymBool>> = c.b.clone().or_else(|| {
                        c.s.as_ref()
                            .map(|s| SymBool::Ne(s.clone(), SymNum::Const(0).rc()).rc())
                    });
                    if let Some(sb) = sel {
                        if av.is_symbolic() || bv.is_symbolic() {
                            let s = SymNum::If(sb, av.as_sym(), bv.as_sym()).rc();
                            vs.push(Value::num(taken_c, Some(s)));
                            pc += 1;
                            continue;
                        }
                    }
                    vs.push(if c.c & 0xffff_ffff != 0 { av } else { bv });
                }
                Operator::LocalGet { local_index } => vs.push(locals[*local_index as usize].clone()),
                Operator::LocalSet { local_index } => {
                    locals[*local_index as usize] = pop!();
                }
                Operator::LocalTee { local_index } => {
                    let v = vs.last().cloned().ok_or_else(|| anyhow!("tee underflow"))?;
                    locals[*local_index as usize] = v;
                }
                Operator::GlobalGet { global_index } => {
                    vs.push(Value::con(self.globals[*global_index as usize] as u64));
                }
                Operator::GlobalSet { global_index } => {
                    self.globals[*global_index as usize] = pop!().c as i64;
                }
                Operator::I32Const { value } => vs.push(Value::con(*value as u32 as u64)),
                Operator::I64Const { value } => vs.push(Value::con(*value as u64)),

                // loads
                Operator::I32Load { memarg } => self.load(&mut vs, memarg, 4, false)?,
                Operator::I64Load { memarg } => self.load(&mut vs, memarg, 8, false)?,
                Operator::I32Load8U { memarg } => self.load(&mut vs, memarg, 1, false)?,
                Operator::I32Load8S { memarg } => self.load(&mut vs, memarg, 1, true)?,
                Operator::I32Load16U { memarg } => self.load(&mut vs, memarg, 2, false)?,
                Operator::I32Load16S { memarg } => self.load(&mut vs, memarg, 2, true)?,
                Operator::I64Load8U { memarg } => self.load(&mut vs, memarg, 1, false)?,
                Operator::I64Load8S { memarg } => self.load(&mut vs, memarg, 1, true)?,
                Operator::I64Load16U { memarg } => self.load(&mut vs, memarg, 2, false)?,
                Operator::I64Load16S { memarg } => self.load(&mut vs, memarg, 2, true)?,
                Operator::I64Load32U { memarg } => self.load(&mut vs, memarg, 4, false)?,
                Operator::I64Load32S { memarg } => self.load(&mut vs, memarg, 4, true)?,

                // stores
                Operator::I32Store { memarg } => self.store(&mut vs, memarg, 4)?,
                Operator::I64Store { memarg } => self.store(&mut vs, memarg, 8)?,
                Operator::I32Store8 { memarg } | Operator::I64Store8 { memarg } => {
                    self.store(&mut vs, memarg, 1)?
                }
                Operator::I32Store16 { memarg } | Operator::I64Store16 { memarg } => {
                    self.store(&mut vs, memarg, 2)?
                }
                Operator::I64Store32 { memarg } => self.store(&mut vs, memarg, 4)?,

                Operator::MemorySize { .. } => {
                    vs.push(Value::con((self.mem.len() / PAGE) as u64));
                }
                Operator::MemoryGrow { .. } => {
                    let pages = pop!().c as usize;
                    let old = self.mem.len() / PAGE;
                    self.mem.resize(self.mem.len() + pages * PAGE, 0);
                    vs.push(Value::con(old as u64));
                }
                Operator::MemoryCopy { .. } => {
                    let n = pop!().c as usize;
                    let src = pop!().c as usize;
                    let dst = pop!().c as usize;
                    self.ensure(src, n)?;
                    self.ensure(dst, n)?;
                    self.mem.copy_within(src..src + n, dst);
                    // move provenance too
                    let moved: Vec<(u32, ByteProv)> = (0..n)
                        .filter_map(|i| {
                            self.tags.get(&((src + i) as u32)).map(|p| ((dst + i) as u32, p.clone()))
                        })
                        .collect();
                    for i in 0..n {
                        self.tags.remove(&((dst + i) as u32));
                    }
                    for (a, p) in moved {
                        self.tags.insert(a, p);
                    }
                }
                Operator::MemoryFill { .. } => {
                    let n = pop!().c as usize;
                    let val = pop!().c as u8;
                    let dst = pop!().c as usize;
                    self.ensure(dst, n)?;
                    for i in 0..n {
                        self.mem[dst + i] = val;
                        self.tags.remove(&((dst + i) as u32));
                    }
                }

                // integer comparisons / arithmetic / conversions
                op => {
                    if !self.exec_numeric(op, &mut vs)? {
                        bail!("unsupported: {:?}", op);
                    }
                }
            }
            pc += 1;
        }
        // fell off the end
        let mut out = Vec::new();
        for _ in 0..n_results {
            out.push(vs.pop().ok_or_else(|| anyhow!("underflow at fn end"))?);
        }
        out.reverse();
        Ok(out)
    }

    fn do_branch(&self, depth: usize, vs: &mut Vec<Value>, cs: &mut Vec<Ctrl>, pc: &mut usize) {
        let ti = cs.len() - 1 - depth;
        let (cont, arity, height, is_loop) = {
            let f = &cs[ti];
            (f.cont, f.arity, f.height, f.is_loop)
        };
        let keep: Vec<Value> = if arity > 0 && vs.len() >= arity {
            vs.split_off(vs.len() - arity)
        } else {
            Vec::new()
        };
        vs.truncate(height);
        vs.extend(keep);
        if is_loop {
            cs.truncate(ti + 1);
        } else {
            cs.truncate(ti);
        }
        *pc = cont;
    }

    /// Load `width` bytes at (addr + memarg.offset); recover a SymNum from
    /// provenance when the bytes are a clean run of one symbolic source.
    fn load(
        &mut self,
        vs: &mut Vec<Value>,
        memarg: &wasmparser::MemArg,
        width: usize,
        signed: bool,
    ) -> Result<()> {
        let addr = vs.pop().ok_or_else(|| anyhow!("addr underflow"))?.c;
        let a = (addr + memarg.offset) as usize;
        self.ensure(a, width)?;
        let mut buf = [0u8; 8];
        buf[..width].copy_from_slice(&self.mem[a..a + width]);
        let mut c = u64::from_le_bytes(buf);
        if signed && width < 8 {
            let shift = 64 - width * 8;
            c = (((c << shift) as i64) >> shift) as u64;
        }
        let sym = self.recover_num(a as u32, width as u32);
        vs.push(Value::num(c, sym));
        Ok(())
    }

    fn recover_num(&self, addr: u32, width: u32) -> Option<Rc<SymNum>> {
        let first = self.tags.get(&addr)?;
        // Byte index of this load within its source. Non-zero means we're
        // reading an interior/high slice of a wider value — e.g. the high 64
        // bits of a u128 read as two i64 words. That must stay symbolic (a
        // shift of the whole), not collapse to a concrete high word.
        let start = first.index;
        let src = first.src.clone();
        // Require a contiguous run [addr, addr+width) from one source with
        // consecutive indices start .. start+width.
        for j in 1..width {
            let p = self.tags.get(&(addr + j))?;
            if !Rc::ptr_eq(&p.src, &src) || p.index != start + j {
                return None;
            }
        }
        // Does the SAME source continue immediately past the load? If so this is
        // a low/interior slice of a wider value and its excess-high bits must be
        // masked off; if not, the load reaches the value's tail.
        let more_after = self
            .tags
            .get(&(addr + width))
            .is_some_and(|p| Rc::ptr_eq(&p.src, &src) && p.index == start + width);

        // The symbolic number for the WHOLE source value.
        let full: Rc<SymNum> = match &*src {
            SymBytes::Le { of, width: sw } => {
                // A full, aligned reload of a stored value is just that value.
                if start == 0 && width == *sw as u32 && !more_after {
                    return Some(of.clone());
                }
                of.clone()
            }
            SymBytes::Storage(_) => SymNum::ULe(src.clone()).rc(),
            _ => return None,
        };
        // Bytes [start, start+width) = (full >> start*8) & (2^(width*8) - 1).
        let shifted = if start == 0 {
            full
        } else {
            SymNum::Shr(full, SymNum::Const(start as u128 * 8).rc()).rc()
        };
        if more_after && width < 16 {
            let mask = (1u128 << (width * 8)) - 1;
            Some(SymNum::And(shifted, SymNum::Const(mask).rc()).rc())
        } else {
            Some(shifted)
        }
    }

    /// Store: pop value then addr; write `width` low bytes. If the value is
    /// symbolic, tag the stored bytes as its little-endian decomposition.
    fn store(&mut self, vs: &mut Vec<Value>, memarg: &wasmparser::MemArg, width: usize) -> Result<()> {
        let val = vs.pop().ok_or_else(|| anyhow!("store underflow"))?;
        let addr = vs.pop().ok_or_else(|| anyhow!("store addr underflow"))?.c;
        let a = (addr + memarg.offset) as usize;
        self.ensure(a, width)?;
        self.mem[a..a + width].copy_from_slice(&val.c.to_le_bytes()[..width]);
        self.clear_tags(a as u32, width as u32);
        if let Some(s) = val.s {
            let src = SymBytes::Le {
                of: s,
                width: width as u8,
            }
            .rc();
            self.tag_bytes(a as u32, &src, width as u32);
        }
        Ok(())
    }

    /// Comparisons, arithmetic, conversions. Returns false if the op isn't
    /// handled (caller bails). Symbolic tags flow through integer arithmetic;
    /// comparisons and bit-counting drop them (result is concrete).
    fn exec_numeric(&mut self, op: &Operator<'static>, vs: &mut Vec<Value>) -> Result<bool> {
        macro_rules! pop {
            () => {
                vs.pop().ok_or_else(|| anyhow!("num underflow"))?
            };
        }
        // symbolic binop: Some(expr) if either operand carries symbolic info
        // (numeric OR boolean — a comparison result used as a number stays
        // symbolic via as_sym's If(cond,1,0), so carries don't leak concretely).
        let sbin = |a: &Value, b: &Value, _mask: u64, f: fn(Rc<SymNum>, Rc<SymNum>) -> SymNum| {
            if !a.is_symbolic() && !b.is_symbolic() {
                return None;
            }
            Some(f(a.as_sym(), b.as_sym()).rc())
        };
        const M32: u64 = 0xffff_ffff;
        const M64: u64 = u64::MAX;

        macro_rules! bin32 {
            ($f:expr, $sym:expr) => {{
                let b = pop!();
                let a = pop!();
                let c = ($f(a.c as u32, b.c as u32) as u64) & M32;
                vs.push(Value::num(c, sbin(&a, &b, M32, $sym)));
                Ok(true)
            }};
            ($f:expr) => {{
                let b = pop!();
                let a = pop!();
                let c = ($f(a.c as u32, b.c as u32) as u64) & M32;
                vs.push(Value::con(c));
                Ok(true)
            }};
        }
        macro_rules! bin64 {
            ($f:expr, $sym:expr) => {{
                let b = pop!();
                let a = pop!();
                let c = $f(a.c, b.c);
                vs.push(Value::num(c, sbin(&a, &b, M64, $sym)));
                Ok(true)
            }};
            ($f:expr) => {{
                let b = pop!();
                let a = pop!();
                let c = $f(a.c, b.c);
                vs.push(Value::con(c));
                Ok(true)
            }};
        }
        // Like bin32!/bin64! but wraps the symbolic result to the type width.
        // The plan IR is u128, yet wasm i32/i64 arithmetic wraps at 2^32 / 2^64,
        // so Add/Sub/Mul/Shl — the ops that can carry past the type width — must
        // mask their symbolic result to stay faithful. Without this the 32-bit-
        // limb u128 multiply's partial products overflow the plan's u128 and the
        // lifted plan no longer matches the bytecode.
        macro_rules! bin32w {
            ($f:expr, $sym:expr) => {{
                let b = pop!();
                let a = pop!();
                let c = ($f(a.c as u32, b.c as u32) as u64) & M32;
                let s = sbin(&a, &b, M32, $sym)
                    .map(|x| SymNum::And(x, SymNum::Const(M32 as u128).rc()).rc());
                vs.push(Value::num(c, s));
                Ok(true)
            }};
        }
        macro_rules! bin64w {
            ($f:expr, $sym:expr) => {{
                let b = pop!();
                let a = pop!();
                let c = $f(a.c, b.c);
                let s = sbin(&a, &b, M64, $sym)
                    .map(|x| SymNum::And(x, SymNum::Const(M64 as u128).rc()).rc());
                vs.push(Value::num(c, s));
                Ok(true)
            }};
        }
        // symbolic bool for a comparison, if either operand is symbolic
        let scmp = |a: &Value, b: &Value, f: fn(Rc<SymNum>, Rc<SymNum>) -> SymBool| {
            if a.s.is_none() && b.s.is_none() {
                return None;
            }
            Some(f(a.as_sym(), b.as_sym()).rc())
        };
        macro_rules! cmp32 {
            ($f:expr, $sym:expr) => {{
                let b = pop!();
                let a = pop!();
                let c = $f(a.c as u32, b.c as u32) as u64;
                vs.push(Value::boolean(c, scmp(&a, &b, $sym)));
                Ok(true)
            }};
            ($f:expr) => {{
                let b = pop!();
                let a = pop!();
                vs.push(Value::con($f(a.c as u32, b.c as u32) as u64));
                Ok(true)
            }};
        }
        macro_rules! cmp64 {
            ($f:expr, $sym:expr) => {{
                let b = pop!();
                let a = pop!();
                let c = $f(a.c, b.c) as u64;
                vs.push(Value::boolean(c, scmp(&a, &b, $sym)));
                Ok(true)
            }};
            ($f:expr) => {{
                let b = pop!();
                let a = pop!();
                vs.push(Value::con($f(a.c, b.c) as u64));
                Ok(true)
            }};
        }

        use SymNum::*;
        match op {
            // i32 arithmetic
            Operator::I32Add => bin32w!(|a: u32, b: u32| a.wrapping_add(b), |a, b| Add(a, b)),
            Operator::I32Sub => bin32w!(|a: u32, b: u32| a.wrapping_sub(b), |a, b| Sub(a, b)),
            Operator::I32Mul => bin32w!(|a: u32, b: u32| a.wrapping_mul(b), |a, b| Mul(a, b)),
            Operator::I32DivU => {
                let b = pop!();
                let a = pop!();
                if b.c as u32 == 0 {
                    bail!("div by zero");
                }
                let c = ((a.c as u32) / (b.c as u32)) as u64;
                vs.push(Value::num(c, sbin(&a, &b, M32, |a, b| Div(a, b))));
                Ok(true)
            }
            Operator::I32RemU => {
                let b = pop!();
                let a = pop!();
                if b.c as u32 == 0 {
                    bail!("rem by zero");
                }
                let c = ((a.c as u32) % (b.c as u32)) as u64;
                vs.push(Value::num(c, sbin(&a, &b, M32, |a, b| Mod(a, b))));
                Ok(true)
            }
            Operator::I32DivS => bin32!(|a: u32, b: u32| (a as i32).wrapping_div(b as i32) as u32),
            Operator::I32RemS => bin32!(|a: u32, b: u32| (a as i32).wrapping_rem((b as i32).max(1)) as u32),
            Operator::I32And => bin32!(|a: u32, b: u32| a & b, |a, b| And(a, b)),
            Operator::I32Or => bin32!(|a: u32, b: u32| a | b, |a, b| Or(a, b)),
            Operator::I32Xor => bin32!(|a: u32, b: u32| a ^ b, |a, b| Xor(a, b)),
            Operator::I32Shl => bin32w!(|a: u32, b: u32| a.wrapping_shl(b), |a, b| Shl(a, b)),
            Operator::I32ShrU => bin32!(|a: u32, b: u32| a.wrapping_shr(b), |a, b| Shr(a, b)),
            Operator::I32ShrS => bin32!(|a: u32, b: u32| (a as i32).wrapping_shr(b) as u32),
            Operator::I32Rotl => bin32!(|a: u32, b: u32| a.rotate_left(b)),
            Operator::I32Rotr => bin32!(|a: u32, b: u32| a.rotate_right(b)),
            // i64 arithmetic
            Operator::I64Add => bin64w!(|a: u64, b: u64| a.wrapping_add(b), |a, b| Add(a, b)),
            Operator::I64Sub => bin64w!(|a: u64, b: u64| a.wrapping_sub(b), |a, b| Sub(a, b)),
            Operator::I64Mul => bin64w!(|a: u64, b: u64| a.wrapping_mul(b), |a, b| Mul(a, b)),
            Operator::I64DivU => {
                let b = pop!();
                let a = pop!();
                if b.c == 0 {
                    bail!("div by zero");
                }
                vs.push(Value::num(a.c / b.c, sbin(&a, &b, M64, |a, b| Div(a, b))));
                Ok(true)
            }
            Operator::I64RemU => {
                let b = pop!();
                let a = pop!();
                if b.c == 0 {
                    bail!("rem by zero");
                }
                vs.push(Value::num(a.c % b.c, sbin(&a, &b, M64, |a, b| Mod(a, b))));
                Ok(true)
            }
            Operator::I64DivS => bin64!(|a: u64, b: u64| (a as i64).wrapping_div((b as i64).max(1)) as u64),
            Operator::I64RemS => bin64!(|a: u64, b: u64| (a as i64).wrapping_rem((b as i64).max(1)) as u64),
            Operator::I64And => bin64!(|a: u64, b: u64| a & b, |a, b| And(a, b)),
            Operator::I64Or => bin64!(|a: u64, b: u64| a | b, |a, b| Or(a, b)),
            Operator::I64Xor => bin64!(|a: u64, b: u64| a ^ b, |a, b| Xor(a, b)),
            Operator::I64Shl => bin64w!(|a: u64, b: u64| a.wrapping_shl(b as u32), |a, b| Shl(a, b)),
            Operator::I64ShrU => bin64!(|a: u64, b: u64| a.wrapping_shr(b as u32), |a, b| Shr(a, b)),
            Operator::I64ShrS => bin64!(|a: u64, b: u64| (a as i64).wrapping_shr(b as u32) as u64),
            Operator::I64Rotl => bin64!(|a: u64, b: u64| a.rotate_left(b as u32)),
            Operator::I64Rotr => bin64!(|a: u64, b: u64| a.rotate_right(b as u32)),
            // comparisons (concrete)
            Operator::I32Eqz => {
                let a = pop!();
                let sb = a
                    .s
                    .as_ref()
                    .map(|s| SymBool::Eq(s.clone(), SymNum::Const(0).rc()).rc());
                vs.push(Value::boolean((a.c as u32 == 0) as u64, sb));
                Ok(true)
            }
            Operator::I64Eqz => {
                let a = pop!();
                let sb = a
                    .s
                    .as_ref()
                    .map(|s| SymBool::Eq(s.clone(), SymNum::Const(0).rc()).rc());
                vs.push(Value::boolean((a.c == 0) as u64, sb));
                Ok(true)
            }
            Operator::I32Eq => cmp32!(|a: u32, b: u32| a == b, |a, b| SymBool::Eq(a, b)),
            Operator::I32Ne => cmp32!(|a: u32, b: u32| a != b, |a, b| SymBool::Ne(a, b)),
            Operator::I32LtU => cmp32!(|a: u32, b: u32| a < b, |a, b| SymBool::Lt(a, b)),
            Operator::I32LtS => cmp32!(|a: u32, b: u32| (a as i32) < (b as i32)),
            Operator::I32GtU => cmp32!(|a: u32, b: u32| a > b, |a, b| SymBool::Gt(a, b)),
            Operator::I32GtS => cmp32!(|a: u32, b: u32| (a as i32) > (b as i32)),
            Operator::I32LeU => cmp32!(|a: u32, b: u32| a <= b, |a, b| SymBool::Lte(a, b)),
            Operator::I32LeS => cmp32!(|a: u32, b: u32| (a as i32) <= (b as i32)),
            Operator::I32GeU => cmp32!(|a: u32, b: u32| a >= b, |a, b| SymBool::Gte(a, b)),
            Operator::I32GeS => cmp32!(|a: u32, b: u32| (a as i32) >= (b as i32)),
            Operator::I64Eq => cmp64!(|a: u64, b: u64| a == b, |a, b| SymBool::Eq(a, b)),
            Operator::I64Ne => cmp64!(|a: u64, b: u64| a != b, |a, b| SymBool::Ne(a, b)),
            Operator::I64LtU => cmp64!(|a: u64, b: u64| a < b, |a, b| SymBool::Lt(a, b)),
            Operator::I64LtS => cmp64!(|a: u64, b: u64| (a as i64) < (b as i64)),
            Operator::I64GtU => cmp64!(|a: u64, b: u64| a > b, |a, b| SymBool::Gt(a, b)),
            Operator::I64GtS => cmp64!(|a: u64, b: u64| (a as i64) > (b as i64)),
            Operator::I64LeU => cmp64!(|a: u64, b: u64| a <= b, |a, b| SymBool::Lte(a, b)),
            Operator::I64LeS => cmp64!(|a: u64, b: u64| (a as i64) <= (b as i64)),
            Operator::I64GeU => cmp64!(|a: u64, b: u64| a >= b, |a, b| SymBool::Gte(a, b)),
            Operator::I64GeS => cmp64!(|a: u64, b: u64| (a as i64) >= (b as i64)),
            // bit counting (concrete)
            Operator::I32Clz => {
                let a = pop!();
                vs.push(Value::con((a.c as u32).leading_zeros() as u64));
                Ok(true)
            }
            Operator::I32Ctz => {
                let a = pop!();
                vs.push(Value::con((a.c as u32).trailing_zeros() as u64));
                Ok(true)
            }
            Operator::I32Popcnt => {
                let a = pop!();
                vs.push(Value::con((a.c as u32).count_ones() as u64));
                Ok(true)
            }
            Operator::I64Clz => {
                let a = pop!();
                vs.push(Value::con(a.c.leading_zeros() as u64));
                Ok(true)
            }
            Operator::I64Ctz => {
                let a = pop!();
                vs.push(Value::con(a.c.trailing_zeros() as u64));
                Ok(true)
            }
            Operator::I64Popcnt => {
                let a = pop!();
                vs.push(Value::con(a.c.count_ones() as u64));
                Ok(true)
            }
            // conversions — value preserved (approx), keep symbolic tag
            Operator::I32WrapI64 => {
                let a = pop!();
                vs.push(Value::num(a.c & M32, a.s));
                Ok(true)
            }
            Operator::I64ExtendI32U => {
                let a = pop!();
                vs.push(Value::num(a.c & M32, a.s));
                Ok(true)
            }
            Operator::I64ExtendI32S => {
                let a = pop!();
                vs.push(Value::num((a.c as u32 as i32 as i64) as u64, a.s));
                Ok(true)
            }
            Operator::I32Extend8S => {
                let a = pop!();
                vs.push(Value::con((a.c as u8 as i8 as i32 as u32) as u64));
                Ok(true)
            }
            Operator::I32Extend16S => {
                let a = pop!();
                vs.push(Value::con((a.c as u16 as i16 as i32 as u32) as u64));
                Ok(true)
            }
            Operator::I64Extend8S => {
                let a = pop!();
                vs.push(Value::con((a.c as u8 as i8 as i64) as u64));
                Ok(true)
            }
            Operator::I64Extend16S => {
                let a = pop!();
                vs.push(Value::con((a.c as u16 as i16 as i64) as u64));
                Ok(true)
            }
            Operator::I64Extend32S => {
                let a = pop!();
                vs.push(Value::con((a.c as u32 as i32 as i64) as u64));
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

/// Precompute matching `end` (and `else`) indices for each structured opener.
fn block_structure(
    body: &[Operator<'static>],
) -> Result<(
    std::collections::HashMap<usize, usize>,
    std::collections::HashMap<usize, usize>,
)> {
    let mut ends = std::collections::HashMap::new();
    let mut elses = std::collections::HashMap::new();
    let mut stack: Vec<usize> = Vec::new();
    for (i, op) in body.iter().enumerate() {
        match op {
            Operator::Block { .. } | Operator::Loop { .. } | Operator::If { .. } => {
                stack.push(i);
            }
            Operator::Else => {
                if let Some(&opener) = stack.last() {
                    elses.insert(opener, i);
                }
            }
            Operator::End => {
                if let Some(opener) = stack.pop() {
                    ends.insert(opener, i);
                }
            }
            _ => {}
        }
    }
    Ok((ends, elses))
}
