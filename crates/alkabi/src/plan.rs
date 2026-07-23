//! View plans: pure expressions over storage keys, calldata, and chain height
//! that reproduce a view method's `CallResponse.data` without executing the
//! contract wasm.
//!
//! Plans are synthesized by the extractor's differential analysis (feature
//! `extract`, `analysis` module): the view runs inside an instrumented
//! interpreter against controlled storage oracles, an expression is fitted to
//! the observed traces, and it is only emitted after matching the wasm
//! byte-for-byte across randomized verification trials. A consumer that holds
//! the contract's storage (e.g. an indexer with a batched get_keys API) can
//! then evaluate the plan instead of simulating — orders of magnitude faster.
//!
//! Semantics:
//!   - all numbers are u128; add/sub/mul wrap; div/mod by zero is an
//!     evaluation error (consumers must fall back to simulate)
//!   - `u` (uint-from-bytes) reads at most the first 16 bytes, little-endian
//!   - `calldata` addresses the post-opcode input words flattened into
//!     16-byte little-endian chunks (the standard word packing)
//!   - a missing storage key evaluates to zero-length bytes
//!   - `loop` concatenates its body for var = 0..count (bounded)

use anyhow::{anyhow, bail, Result};

pub const PLAN_VERSION: u32 = 1;

/// Safety bound on loop iterations during evaluation.
pub const LOOP_LIMIT: u128 = 65_536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BytesExpr {
    /// Constant bytes.
    Const(Vec<u8>),
    /// The storage value at a (computed) key.
    Storage(Box<BytesExpr>),
    /// Bytes `start..start+len` of the flattened post-opcode calldata words
    /// (`len` = None means "to the end").
    Calldata { start: u32, len: Option<u32> },
    Concat(Vec<BytesExpr>),
    Slice {
        of: Box<BytesExpr>,
        start: Box<NumExpr>,
        len: Box<NumExpr>,
    },
    /// Little-endian encoding of a number, `width` bytes.
    Le { of: Box<NumExpr>, width: u8 },
    If {
        cond: Box<BoolExpr>,
        then: Box<BytesExpr>,
        r#else: Box<BytesExpr>,
    },
    /// Concatenation of `body` evaluated for var = 0..count.
    Loop {
        count: Box<NumExpr>,
        body: Box<BytesExpr>,
    },
    /// ASCII lowercase hex of the inner bytes.
    Hex(Box<BytesExpr>),
    /// ASCII decimal of a number.
    Decimal(Box<NumExpr>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NumExpr {
    Const(u128),
    /// Post-opcode input word `i`.
    Word(u32),
    /// Little-endian uint of (at most the first 16) bytes.
    ULe(Box<BytesExpr>),
    Len(Box<BytesExpr>),
    Height,
    /// The innermost loop variable.
    Var,
    Add(Box<NumExpr>, Box<NumExpr>),
    Sub(Box<NumExpr>, Box<NumExpr>),
    Mul(Box<NumExpr>, Box<NumExpr>),
    Div(Box<NumExpr>, Box<NumExpr>),
    Mod(Box<NumExpr>, Box<NumExpr>),
    /// Bitwise / shift, matching the wasm integer ops (u128 semantics; shift
    /// amount ≥ 128 yields 0). Needed to lower lifted computations like
    /// `base >> halving_epoch`.
    Shr(Box<NumExpr>, Box<NumExpr>),
    Shl(Box<NumExpr>, Box<NumExpr>),
    And(Box<NumExpr>, Box<NumExpr>),
    Or(Box<NumExpr>, Box<NumExpr>),
    Xor(Box<NumExpr>, Box<NumExpr>),
    /// Conditional value — the numeric analogue of `BytesExpr::If`. Lets the
    /// lifter capture branchless conditionals (`select`, `saturating_sub`,
    /// `min`/`max`) in a single pass.
    If {
        cond: Box<BoolExpr>,
        then: Box<NumExpr>,
        r#else: Box<NumExpr>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoolExpr {
    Eq(Box<NumExpr>, Box<NumExpr>),
    Ne(Box<NumExpr>, Box<NumExpr>),
    Lt(Box<NumExpr>, Box<NumExpr>),
    Lte(Box<NumExpr>, Box<NumExpr>),
    Gt(Box<NumExpr>, Box<NumExpr>),
    Gte(Box<NumExpr>, Box<NumExpr>),
    BytesEq(Box<BytesExpr>, Box<BytesExpr>),
    And(Vec<BoolExpr>),
    Or(Vec<BoolExpr>),
    Not(Box<BoolExpr>),
}

/// A verified plan attached to a view method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub expr: BytesExpr,
    /// Number of randomized differential trials the plan survived against the
    /// wasm. Plans are probabilistically verified, not proven.
    pub trials: u32,
}

/*──────────────────────── JSON writing ────────────────────────*/

fn push_hex(out: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push('"');
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out.push('"');
}

impl BytesExpr {
    pub fn write_json(&self, out: &mut String) {
        match self {
            BytesExpr::Const(bytes) => {
                out.push_str("{\"bytes\":");
                push_hex(out, bytes);
                out.push('}');
            }
            BytesExpr::Storage(key) => {
                out.push_str("{\"storage\":");
                key.write_json(out);
                out.push('}');
            }
            BytesExpr::Calldata { start, len } => {
                out.push_str("{\"calldata\":{\"start\":");
                out.push_str(&start.to_string());
                if let Some(len) = len {
                    out.push_str(",\"len\":");
                    out.push_str(&len.to_string());
                }
                out.push_str("}}");
            }
            BytesExpr::Concat(parts) => {
                out.push_str("{\"concat\":[");
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    part.write_json(out);
                }
                out.push_str("]}");
            }
            BytesExpr::Slice { of, start, len } => {
                out.push_str("{\"slice\":{\"of\":");
                of.write_json(out);
                out.push_str(",\"start\":");
                start.write_json(out);
                out.push_str(",\"len\":");
                len.write_json(out);
                out.push_str("}}");
            }
            BytesExpr::Le { of, width } => {
                out.push_str("{\"le\":{\"of\":");
                of.write_json(out);
                out.push_str(",\"width\":");
                out.push_str(&width.to_string());
                out.push_str("}}");
            }
            BytesExpr::If { cond, then, r#else } => {
                out.push_str("{\"if\":{\"cond\":");
                cond.write_json(out);
                out.push_str(",\"then\":");
                then.write_json(out);
                out.push_str(",\"else\":");
                r#else.write_json(out);
                out.push_str("}}");
            }
            BytesExpr::Loop { count, body } => {
                out.push_str("{\"loop\":{\"count\":");
                count.write_json(out);
                out.push_str(",\"body\":");
                body.write_json(out);
                out.push_str("}}");
            }
            BytesExpr::Hex(inner) => {
                out.push_str("{\"hex\":");
                inner.write_json(out);
                out.push('}');
            }
            BytesExpr::Decimal(inner) => {
                out.push_str("{\"decimal\":");
                inner.write_json(out);
                out.push('}');
            }
        }
    }
}

impl NumExpr {
    pub fn write_json(&self, out: &mut String) {
        match self {
            NumExpr::Const(n) => {
                out.push_str("{\"num\":\"");
                out.push_str(&n.to_string());
                out.push_str("\"}");
            }
            NumExpr::Word(i) => {
                out.push_str("{\"word\":");
                out.push_str(&i.to_string());
                out.push('}');
            }
            NumExpr::ULe(bytes) => {
                out.push_str("{\"u\":");
                bytes.write_json(out);
                out.push('}');
            }
            NumExpr::Len(bytes) => {
                out.push_str("{\"len\":");
                bytes.write_json(out);
                out.push('}');
            }
            NumExpr::Height => out.push_str("{\"height\":{}}"),
            NumExpr::Var => out.push_str("{\"var\":{}}"),
            NumExpr::Add(a, b) => write_num_pair(out, "add", a, b),
            NumExpr::Sub(a, b) => write_num_pair(out, "sub", a, b),
            NumExpr::Mul(a, b) => write_num_pair(out, "mul", a, b),
            NumExpr::Div(a, b) => write_num_pair(out, "div", a, b),
            NumExpr::Mod(a, b) => write_num_pair(out, "mod", a, b),
            NumExpr::Shr(a, b) => write_num_pair(out, "shr", a, b),
            NumExpr::Shl(a, b) => write_num_pair(out, "shl", a, b),
            NumExpr::And(a, b) => write_num_pair(out, "and", a, b),
            NumExpr::Or(a, b) => write_num_pair(out, "or", a, b),
            NumExpr::Xor(a, b) => write_num_pair(out, "xor", a, b),
            NumExpr::If { cond, then, r#else } => {
                out.push_str("{\"nif\":{\"cond\":");
                cond.write_json(out);
                out.push_str(",\"then\":");
                then.write_json(out);
                out.push_str(",\"else\":");
                r#else.write_json(out);
                out.push_str("}}");
            }
        }
    }
}

fn write_num_pair(out: &mut String, op: &str, a: &NumExpr, b: &NumExpr) {
    out.push_str("{\"");
    out.push_str(op);
    out.push_str("\":[");
    a.write_json(out);
    out.push(',');
    b.write_json(out);
    out.push_str("]}");
}

impl BoolExpr {
    pub fn write_json(&self, out: &mut String) {
        let pair = |out: &mut String, op: &str, a: &NumExpr, b: &NumExpr| {
            out.push_str("{\"");
            out.push_str(op);
            out.push_str("\":[");
            a.write_json(out);
            out.push(',');
            b.write_json(out);
            out.push_str("]}");
        };
        match self {
            BoolExpr::Eq(a, b) => pair(out, "eq", a, b),
            BoolExpr::Ne(a, b) => pair(out, "ne", a, b),
            BoolExpr::Lt(a, b) => pair(out, "lt", a, b),
            BoolExpr::Lte(a, b) => pair(out, "lte", a, b),
            BoolExpr::Gt(a, b) => pair(out, "gt", a, b),
            BoolExpr::Gte(a, b) => pair(out, "gte", a, b),
            BoolExpr::BytesEq(a, b) => {
                out.push_str("{\"beq\":[");
                a.write_json(out);
                out.push(',');
                b.write_json(out);
                out.push_str("]}");
            }
            BoolExpr::And(parts) => {
                out.push_str("{\"and\":[");
                for (i, p) in parts.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    p.write_json(out);
                }
                out.push_str("]}");
            }
            BoolExpr::Or(parts) => {
                out.push_str("{\"or\":[");
                for (i, p) in parts.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    p.write_json(out);
                }
                out.push_str("]}");
            }
            BoolExpr::Not(inner) => {
                out.push_str("{\"not\":");
                inner.write_json(out);
                out.push('}');
            }
        }
    }
}

impl Plan {
    pub fn write_json(&self, out: &mut String) {
        out.push_str("{\"v\":");
        out.push_str(&PLAN_VERSION.to_string());
        out.push_str(",\"expr\":");
        self.expr.write_json(out);
        out.push_str(",\"trials\":");
        out.push_str(&self.trials.to_string());
        out.push('}');
    }

    pub fn to_json(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }
}

/*──────────────────────── evaluation ────────────────────────*/

/// Everything a plan may read from the outside world.
pub trait PlanEnv {
    /// Storage value for a key; missing keys are zero-length.
    fn storage(&mut self, key: &[u8]) -> Vec<u8>;
    fn height(&self) -> u64;
    /// Post-opcode input words.
    fn words(&self) -> &[u128];
}

fn calldata_bytes(env: &mut dyn PlanEnv) -> Vec<u8> {
    env.words()
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect()
}

pub fn eval_bytes(
    expr: &BytesExpr,
    env: &mut dyn PlanEnv,
    var: Option<u128>,
) -> Result<Vec<u8>> {
    match expr {
        BytesExpr::Const(bytes) => Ok(bytes.clone()),
        BytesExpr::Storage(key) => {
            let key = eval_bytes(key, env, var)?;
            Ok(env.storage(&key))
        }
        BytesExpr::Calldata { start, len } => {
            let all = calldata_bytes(env);
            let start = *start as usize;
            if start > all.len() {
                bail!("plan: calldata start {} out of range", start);
            }
            let end = match len {
                Some(len) => (start + *len as usize).min(all.len()),
                None => all.len(),
            };
            Ok(all[start..end].to_vec())
        }
        BytesExpr::Concat(parts) => {
            let mut out = Vec::new();
            for part in parts {
                out.extend(eval_bytes(part, env, var)?);
            }
            Ok(out)
        }
        BytesExpr::Slice { of, start, len } => {
            let bytes = eval_bytes(of, env, var)?;
            let start = eval_num(start, env, var)? as usize;
            let len = eval_num(len, env, var)? as usize;
            if start > bytes.len() || start + len > bytes.len() {
                bail!(
                    "plan: slice {}..{} out of range (len {})",
                    start,
                    start + len,
                    bytes.len()
                );
            }
            Ok(bytes[start..start + len].to_vec())
        }
        BytesExpr::Le { of, width } => {
            let n = eval_num(of, env, var)?;
            Ok(n.to_le_bytes()[..*width as usize].to_vec())
        }
        BytesExpr::If { cond, then, r#else } => {
            if eval_bool(cond, env, var)? {
                eval_bytes(then, env, var)
            } else {
                eval_bytes(r#else, env, var)
            }
        }
        BytesExpr::Loop { count, body } => {
            let count = eval_num(count, env, var)?;
            if count > LOOP_LIMIT {
                bail!("plan: loop count {} exceeds limit", count);
            }
            let mut out = Vec::new();
            for i in 0..count {
                out.extend(eval_bytes(body, env, Some(i))?);
            }
            Ok(out)
        }
        BytesExpr::Hex(inner) => {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            let bytes = eval_bytes(inner, env, var)?;
            let mut out = Vec::with_capacity(bytes.len() * 2);
            for b in bytes {
                out.push(HEX[(b >> 4) as usize]);
                out.push(HEX[(b & 0xf) as usize]);
            }
            Ok(out)
        }
        BytesExpr::Decimal(inner) => {
            let n = eval_num(inner, env, var)?;
            Ok(n.to_string().into_bytes())
        }
    }
}

pub fn eval_num(expr: &NumExpr, env: &mut dyn PlanEnv, var: Option<u128>) -> Result<u128> {
    match expr {
        NumExpr::Const(n) => Ok(*n),
        NumExpr::Word(i) => env
            .words()
            .get(*i as usize)
            .copied()
            .ok_or_else(|| anyhow!("plan: input word {} missing", i)),
        NumExpr::ULe(bytes) => {
            let bytes = eval_bytes(bytes, env, var)?;
            let take = bytes.len().min(16);
            let mut buf = [0u8; 16];
            buf[..take].copy_from_slice(&bytes[..take]);
            Ok(u128::from_le_bytes(buf))
        }
        NumExpr::Len(bytes) => Ok(eval_bytes(bytes, env, var)?.len() as u128),
        NumExpr::Height => Ok(env.height() as u128),
        NumExpr::Var => var.ok_or_else(|| anyhow!("plan: `var` used outside a loop")),
        NumExpr::Add(a, b) => Ok(eval_num(a, env, var)?.wrapping_add(eval_num(b, env, var)?)),
        NumExpr::Sub(a, b) => Ok(eval_num(a, env, var)?.wrapping_sub(eval_num(b, env, var)?)),
        NumExpr::Mul(a, b) => Ok(eval_num(a, env, var)?.wrapping_mul(eval_num(b, env, var)?)),
        NumExpr::Div(a, b) => {
            let d = eval_num(b, env, var)?;
            if d == 0 {
                bail!("plan: division by zero");
            }
            Ok(eval_num(a, env, var)? / d)
        }
        NumExpr::Mod(a, b) => {
            let d = eval_num(b, env, var)?;
            if d == 0 {
                bail!("plan: modulo by zero");
            }
            Ok(eval_num(a, env, var)? % d)
        }
        NumExpr::Shr(a, b) => {
            let sh = eval_num(b, env, var)?;
            let a = eval_num(a, env, var)?;
            Ok(if sh >= 128 { 0 } else { a >> sh })
        }
        NumExpr::Shl(a, b) => {
            let sh = eval_num(b, env, var)?;
            let a = eval_num(a, env, var)?;
            Ok(if sh >= 128 { 0 } else { a.wrapping_shl(sh as u32) })
        }
        NumExpr::And(a, b) => Ok(eval_num(a, env, var)? & eval_num(b, env, var)?),
        NumExpr::Or(a, b) => Ok(eval_num(a, env, var)? | eval_num(b, env, var)?),
        NumExpr::Xor(a, b) => Ok(eval_num(a, env, var)? ^ eval_num(b, env, var)?),
        NumExpr::If { cond, then, r#else } => {
            if eval_bool(cond, env, var)? {
                eval_num(then, env, var)
            } else {
                eval_num(r#else, env, var)
            }
        }
    }
}

pub fn eval_bool(expr: &BoolExpr, env: &mut dyn PlanEnv, var: Option<u128>) -> Result<bool> {
    Ok(match expr {
        BoolExpr::Eq(a, b) => eval_num(a, env, var)? == eval_num(b, env, var)?,
        BoolExpr::Ne(a, b) => eval_num(a, env, var)? != eval_num(b, env, var)?,
        BoolExpr::Lt(a, b) => eval_num(a, env, var)? < eval_num(b, env, var)?,
        BoolExpr::Lte(a, b) => eval_num(a, env, var)? <= eval_num(b, env, var)?,
        BoolExpr::Gt(a, b) => eval_num(a, env, var)? > eval_num(b, env, var)?,
        BoolExpr::Gte(a, b) => eval_num(a, env, var)? >= eval_num(b, env, var)?,
        BoolExpr::BytesEq(a, b) => eval_bytes(a, env, var)? == eval_bytes(b, env, var)?,
        BoolExpr::And(parts) => {
            for p in parts {
                if !eval_bool(p, env, var)? {
                    return Ok(false);
                }
            }
            true
        }
        BoolExpr::Or(parts) => {
            for p in parts {
                if eval_bool(p, env, var)? {
                    return Ok(true);
                }
            }
            false
        }
        BoolExpr::Not(inner) => !eval_bool(inner, env, var)?,
    })
}

/// Evaluate a plan into the method's `CallResponse.data` bytes.
pub fn eval_plan(plan: &Plan, env: &mut dyn PlanEnv) -> Result<Vec<u8>> {
    eval_bytes(&plan.expr, env, None)
}

/*──────────────────── JSON parsing (host side) ────────────────────*/

#[cfg(feature = "extract")]
mod parse {
    use super::*;
    use serde_json::Value;

    fn hex_bytes(v: &Value) -> Result<Vec<u8>> {
        let s = v.as_str().ok_or_else(|| anyhow!("expected hex string"))?;
        if s.len() % 2 != 0 {
            bail!("odd-length hex string");
        }
        (0..s.len())
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| anyhow!("invalid hex"))
            })
            .collect()
    }

    fn num_field(v: &Value, key: &str) -> Result<u64> {
        v.get(key)
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("plan: expected numeric \"{}\"", key))
    }

    fn pair(v: &Value) -> Result<(NumExpr, NumExpr)> {
        let arr = v.as_array().ok_or_else(|| anyhow!("expected [a, b]"))?;
        if arr.len() != 2 {
            bail!("expected exactly two operands");
        }
        Ok((parse_num(&arr[0])?, parse_num(&arr[1])?))
    }

    pub fn parse_bytes(v: &Value) -> Result<BytesExpr> {
        let obj = v.as_object().ok_or_else(|| anyhow!("plan: expected object"))?;
        if let Some(inner) = obj.get("bytes") {
            return Ok(BytesExpr::Const(hex_bytes(inner)?));
        }
        if let Some(inner) = obj.get("storage") {
            return Ok(BytesExpr::Storage(Box::new(parse_bytes(inner)?)));
        }
        if let Some(inner) = obj.get("calldata") {
            return Ok(BytesExpr::Calldata {
                start: num_field(inner, "start")? as u32,
                len: inner.get("len").and_then(Value::as_u64).map(|l| l as u32),
            });
        }
        if let Some(inner) = obj.get("concat") {
            let arr = inner.as_array().ok_or_else(|| anyhow!("concat: array"))?;
            return Ok(BytesExpr::Concat(
                arr.iter().map(parse_bytes).collect::<Result<_>>()?,
            ));
        }
        if let Some(inner) = obj.get("slice") {
            return Ok(BytesExpr::Slice {
                of: Box::new(parse_bytes(
                    inner.get("of").ok_or_else(|| anyhow!("slice: of"))?,
                )?),
                start: Box::new(parse_num(
                    inner.get("start").ok_or_else(|| anyhow!("slice: start"))?,
                )?),
                len: Box::new(parse_num(
                    inner.get("len").ok_or_else(|| anyhow!("slice: len"))?,
                )?),
            });
        }
        if let Some(inner) = obj.get("le") {
            return Ok(BytesExpr::Le {
                of: Box::new(parse_num(
                    inner.get("of").ok_or_else(|| anyhow!("le: of"))?,
                )?),
                width: num_field(inner, "width")? as u8,
            });
        }
        if let Some(inner) = obj.get("if") {
            return Ok(BytesExpr::If {
                cond: Box::new(parse_bool(
                    inner.get("cond").ok_or_else(|| anyhow!("if: cond"))?,
                )?),
                then: Box::new(parse_bytes(
                    inner.get("then").ok_or_else(|| anyhow!("if: then"))?,
                )?),
                r#else: Box::new(parse_bytes(
                    inner.get("else").ok_or_else(|| anyhow!("if: else"))?,
                )?),
            });
        }
        if let Some(inner) = obj.get("loop") {
            return Ok(BytesExpr::Loop {
                count: Box::new(parse_num(
                    inner.get("count").ok_or_else(|| anyhow!("loop: count"))?,
                )?),
                body: Box::new(parse_bytes(
                    inner.get("body").ok_or_else(|| anyhow!("loop: body"))?,
                )?),
            });
        }
        if let Some(inner) = obj.get("hex") {
            return Ok(BytesExpr::Hex(Box::new(parse_bytes(inner)?)));
        }
        if let Some(inner) = obj.get("decimal") {
            return Ok(BytesExpr::Decimal(Box::new(parse_num(inner)?)));
        }
        bail!("plan: unrecognized bytes expression: {}", v)
    }

    pub fn parse_num(v: &Value) -> Result<NumExpr> {
        let obj = v.as_object().ok_or_else(|| anyhow!("plan: expected object"))?;
        if let Some(inner) = obj.get("num") {
            let s = inner.as_str().ok_or_else(|| anyhow!("num: string"))?;
            return Ok(NumExpr::Const(
                s.parse().map_err(|_| anyhow!("num: invalid u128"))?,
            ));
        }
        if let Some(inner) = obj.get("word") {
            return Ok(NumExpr::Word(
                inner.as_u64().ok_or_else(|| anyhow!("word: index"))? as u32,
            ));
        }
        if let Some(inner) = obj.get("u") {
            return Ok(NumExpr::ULe(Box::new(parse_bytes(inner)?)));
        }
        if let Some(inner) = obj.get("len") {
            return Ok(NumExpr::Len(Box::new(parse_bytes(inner)?)));
        }
        if obj.contains_key("height") {
            return Ok(NumExpr::Height);
        }
        if obj.contains_key("var") {
            return Ok(NumExpr::Var);
        }
        for (op, build) in [
            ("add", NumExpr::Add as fn(Box<NumExpr>, Box<NumExpr>) -> NumExpr),
            ("sub", NumExpr::Sub),
            ("mul", NumExpr::Mul),
            ("div", NumExpr::Div),
            ("mod", NumExpr::Mod),
            ("shr", NumExpr::Shr),
            ("shl", NumExpr::Shl),
            ("and", NumExpr::And),
            ("or", NumExpr::Or),
            ("xor", NumExpr::Xor),
        ] {
            if let Some(inner) = obj.get(op) {
                let (a, b) = pair(inner)?;
                return Ok(build(Box::new(a), Box::new(b)));
            }
        }
        if let Some(inner) = obj.get("nif") {
            return Ok(NumExpr::If {
                cond: Box::new(parse_bool(inner.get("cond").ok_or_else(|| anyhow!("nif: cond"))?)?),
                then: Box::new(parse_num(inner.get("then").ok_or_else(|| anyhow!("nif: then"))?)?),
                r#else: Box::new(parse_num(inner.get("else").ok_or_else(|| anyhow!("nif: else"))?)?),
            });
        }
        bail!("plan: unrecognized number expression: {}", v)
    }

    pub fn parse_bool(v: &Value) -> Result<BoolExpr> {
        let obj = v.as_object().ok_or_else(|| anyhow!("plan: expected object"))?;
        for (op, build) in [
            ("eq", BoolExpr::Eq as fn(Box<NumExpr>, Box<NumExpr>) -> BoolExpr),
            ("ne", BoolExpr::Ne),
            ("lt", BoolExpr::Lt),
            ("lte", BoolExpr::Lte),
            ("gt", BoolExpr::Gt),
            ("gte", BoolExpr::Gte),
        ] {
            if let Some(inner) = obj.get(op) {
                let (a, b) = pair(inner)?;
                return Ok(build(Box::new(a), Box::new(b)));
            }
        }
        if let Some(inner) = obj.get("beq") {
            let arr = inner.as_array().ok_or_else(|| anyhow!("beq: array"))?;
            if arr.len() != 2 {
                bail!("beq: two operands");
            }
            return Ok(BoolExpr::BytesEq(
                Box::new(parse_bytes(&arr[0])?),
                Box::new(parse_bytes(&arr[1])?),
            ));
        }
        if let Some(inner) = obj.get("and") {
            let arr = inner.as_array().ok_or_else(|| anyhow!("and: array"))?;
            return Ok(BoolExpr::And(
                arr.iter().map(parse_bool).collect::<Result<_>>()?,
            ));
        }
        if let Some(inner) = obj.get("or") {
            let arr = inner.as_array().ok_or_else(|| anyhow!("or: array"))?;
            return Ok(BoolExpr::Or(
                arr.iter().map(parse_bool).collect::<Result<_>>()?,
            ));
        }
        if let Some(inner) = obj.get("not") {
            return Ok(BoolExpr::Not(Box::new(parse_bool(inner)?)));
        }
        bail!("plan: unrecognized bool expression: {}", v)
    }

    pub fn parse_plan(v: &Value) -> Result<Plan> {
        let expr = parse_bytes(v.get("expr").ok_or_else(|| anyhow!("plan: missing expr"))?)?;
        let trials = v.get("trials").and_then(Value::as_u64).unwrap_or(0) as u32;
        Ok(Plan { expr, trials })
    }
}

#[cfg(feature = "extract")]
pub use parse::{parse_bool, parse_bytes, parse_num, parse_plan};
