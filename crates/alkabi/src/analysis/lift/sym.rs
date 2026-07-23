//! Symbolic values for the concolic lifter.
//!
//! A runtime value carries a concrete u64 plus an optional symbolic tag
//! (`SymNum`). Memory carries, per byte, an optional provenance (`ByteProv`)
//! saying which symbolic byte-string that byte came from. Contiguous
//! same-source provenance runs are recovered into `SymBytes`, and both lower to
//! the plan IR (`NumExpr` / `BytesExpr`). Anything the lifter can't represent
//! stays `None` (concrete) — and only the parts that flow into `response.data`
//! matter, so untracked scratch computation is harmless.

use crate::plan::{BytesExpr, NumExpr};
use std::rc::Rc;

/// Constant-fold and simplify a lifted expression: kills the `0 << x`, `0 | y`,
/// `x + 0`, `le(const)` noise that LLVM's lowering leaves behind, so a lifted
/// plan reads as cleanly as a template one.
pub fn simplify_bytes(e: BytesExpr) -> BytesExpr {
    match e {
        BytesExpr::Concat(parts) => {
            let mut out: Vec<BytesExpr> = Vec::new();
            for p in parts {
                let p = simplify_bytes(p);
                if let BytesExpr::Const(b) = &p {
                    if b.is_empty() {
                        continue;
                    }
                    if let Some(BytesExpr::Const(prev)) = out.last_mut() {
                        prev.extend_from_slice(b);
                        continue;
                    }
                }
                if let BytesExpr::Concat(inner) = p {
                    out.extend(inner);
                } else {
                    out.push(p);
                }
            }
            if out.len() == 1 {
                out.pop().unwrap()
            } else {
                BytesExpr::Concat(out)
            }
        }
        BytesExpr::Storage(k) => BytesExpr::Storage(Box::new(simplify_bytes(*k))),
        BytesExpr::Le { of, width } => {
            let of = simplify_num(*of);
            if let NumExpr::Const(n) = of {
                return BytesExpr::Const(n.to_le_bytes()[..width as usize].to_vec());
            }
            BytesExpr::Le {
                of: Box::new(of),
                width,
            }
        }
        BytesExpr::Slice { of, start, len } => BytesExpr::Slice {
            of: Box::new(simplify_bytes(*of)),
            start: Box::new(simplify_num(*start)),
            len: Box::new(simplify_num(*len)),
        },
        other => other,
    }
}

pub fn simplify_num(e: NumExpr) -> NumExpr {
    use NumExpr::*;
    let bin = |a: NumExpr, b: NumExpr| (simplify_num(a), simplify_num(b));
    match e {
        Add(a, b) => match bin(*a, *b) {
            (Const(x), Const(y)) => Const(x.wrapping_add(y)),
            (Const(0), y) => y,
            (x, Const(0)) => x,
            (x, y) => Add(Box::new(x), Box::new(y)),
        },
        Sub(a, b) => match bin(*a, *b) {
            (Const(x), Const(y)) => Const(x.wrapping_sub(y)),
            (x, Const(0)) => x,
            (x, y) => Sub(Box::new(x), Box::new(y)),
        },
        Mul(a, b) => match bin(*a, *b) {
            (Const(x), Const(y)) => Const(x.wrapping_mul(y)),
            (Const(0), _) | (_, Const(0)) => Const(0),
            (Const(1), y) => y,
            (x, Const(1)) => x,
            (x, y) => Mul(Box::new(x), Box::new(y)),
        },
        Div(a, b) => match bin(*a, *b) {
            (Const(x), Const(y)) if y != 0 => Const(x / y),
            (x, y) => Div(Box::new(x), Box::new(y)),
        },
        Mod(a, b) => match bin(*a, *b) {
            (Const(x), Const(y)) if y != 0 => Const(x % y),
            (x, y) => Mod(Box::new(x), Box::new(y)),
        },
        Shr(a, b) => match bin(*a, *b) {
            (Const(0), _) => Const(0),
            (x, Const(0)) => x,
            (Const(x), Const(s)) if s < 128 => Const(x >> s),
            (x, y) => Shr(Box::new(x), Box::new(y)),
        },
        Shl(a, b) => match bin(*a, *b) {
            (Const(0), _) => Const(0),
            (x, Const(0)) => x,
            (Const(x), Const(s)) if s < 128 => Const(x.wrapping_shl(s as u32)),
            (x, y) => Shl(Box::new(x), Box::new(y)),
        },
        Or(a, b) => match bin(*a, *b) {
            (Const(0), y) => y,
            (x, Const(0)) => x,
            (Const(x), Const(y)) => Const(x | y),
            (x, y) => Or(Box::new(x), Box::new(y)),
        },
        And(a, b) => match bin(*a, *b) {
            (Const(0), _) | (_, Const(0)) => Const(0),
            (Const(x), Const(y)) => Const(x & y),
            (x, y) => And(Box::new(x), Box::new(y)),
        },
        Xor(a, b) => match bin(*a, *b) {
            (Const(x), Const(y)) => Const(x ^ y),
            (x, y) => Xor(Box::new(x), Box::new(y)),
        },
        ULe(b) => ULe(Box::new(simplify_bytes(*b))),
        Len(b) => Len(Box::new(simplify_bytes(*b))),
        leaf => leaf,
    }
}

/// A symbolic byte-string (mirrors `BytesExpr`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymBytes {
    Const(Vec<u8>),
    Storage(Rc<SymBytes>),
    Calldata { start: u32, len: Option<u32> },
    Concat(Vec<Rc<SymBytes>>),
    Le { of: Rc<SymNum>, width: u8 },
}

/// A symbolic number (mirrors `NumExpr`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymNum {
    Const(u128),
    ULe(Rc<SymBytes>),
    Len(Rc<SymBytes>),
    Height,
    Word(u32),
    Add(Rc<SymNum>, Rc<SymNum>),
    Sub(Rc<SymNum>, Rc<SymNum>),
    Mul(Rc<SymNum>, Rc<SymNum>),
    Div(Rc<SymNum>, Rc<SymNum>),
    Mod(Rc<SymNum>, Rc<SymNum>),
    /// x >> n  (== x / 2^n); lowered as Div by a power-of-two constant when n
    /// is constant, else via repeated halving is not representable — kept for
    /// the common constant-shift case.
    Shr(Rc<SymNum>, Rc<SymNum>),
    Shl(Rc<SymNum>, Rc<SymNum>),
    And(Rc<SymNum>, Rc<SymNum>),
    Or(Rc<SymNum>, Rc<SymNum>),
    Xor(Rc<SymNum>, Rc<SymNum>),
}

/// Provenance of a single memory byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteProv {
    pub src: Rc<SymBytes>,
    pub index: u32,
}

impl SymBytes {
    pub fn rc(self) -> Rc<SymBytes> {
        Rc::new(self)
    }
}
impl SymNum {
    pub fn rc(self) -> Rc<SymNum> {
        Rc::new(self)
    }
}

/*──────────────────────── lowering to plan IR ────────────────────────*/

impl SymNum {
    pub fn lower(&self) -> Option<NumExpr> {
        Some(match self {
            SymNum::Const(n) => NumExpr::Const(*n),
            SymNum::ULe(b) => NumExpr::ULe(Box::new(b.lower()?)),
            SymNum::Len(b) => NumExpr::Len(Box::new(b.lower()?)),
            SymNum::Height => NumExpr::Height,
            SymNum::Word(i) => NumExpr::Word(*i),
            SymNum::Add(a, b) => NumExpr::Add(Box::new(a.lower()?), Box::new(b.lower()?)),
            SymNum::Sub(a, b) => NumExpr::Sub(Box::new(a.lower()?), Box::new(b.lower()?)),
            SymNum::Mul(a, b) => NumExpr::Mul(Box::new(a.lower()?), Box::new(b.lower()?)),
            SymNum::Div(a, b) => NumExpr::Div(Box::new(a.lower()?), Box::new(b.lower()?)),
            SymNum::Mod(a, b) => NumExpr::Mod(Box::new(a.lower()?), Box::new(b.lower()?)),
            SymNum::Shr(a, b) => NumExpr::Shr(Box::new(a.lower()?), Box::new(b.lower()?)),
            SymNum::Shl(a, b) => NumExpr::Shl(Box::new(a.lower()?), Box::new(b.lower()?)),
            SymNum::And(a, b) => NumExpr::And(Box::new(a.lower()?), Box::new(b.lower()?)),
            SymNum::Or(a, b) => NumExpr::Or(Box::new(a.lower()?), Box::new(b.lower()?)),
            SymNum::Xor(a, b) => NumExpr::Xor(Box::new(a.lower()?), Box::new(b.lower()?)),
        })
    }
}

impl SymBytes {
    pub fn lower(&self) -> Option<BytesExpr> {
        Some(match self {
            SymBytes::Const(b) => BytesExpr::Const(b.clone()),
            SymBytes::Storage(k) => BytesExpr::Storage(Box::new(k.lower()?)),
            SymBytes::Calldata { start, len } => BytesExpr::Calldata {
                start: *start,
                len: *len,
            },
            SymBytes::Concat(parts) => {
                let mut out = Vec::with_capacity(parts.len());
                for p in parts {
                    out.push(p.lower()?);
                }
                BytesExpr::Concat(out)
            }
            SymBytes::Le { of, width } => BytesExpr::Le {
                of: Box::new(of.lower()?),
                width: *width,
            },
        })
    }

    /// Flatten nested concats and merge adjacent constants for a tidy plan.
    pub fn normalize(parts: Vec<Rc<SymBytes>>) -> SymBytes {
        let mut flat: Vec<Rc<SymBytes>> = Vec::new();
        for p in parts {
            match &*p {
                SymBytes::Concat(inner) => flat.extend(inner.iter().cloned()),
                _ => flat.push(p),
            }
        }
        let mut merged: Vec<Rc<SymBytes>> = Vec::new();
        for p in flat {
            if let (Some(last), SymBytes::Const(b)) = (merged.last().cloned(), &*p) {
                if let SymBytes::Const(prev) = &*last {
                    let mut joined = prev.clone();
                    joined.extend_from_slice(b);
                    *merged.last_mut().unwrap() = SymBytes::Const(joined).rc();
                    continue;
                }
            }
            merged.push(p);
        }
        if merged.len() == 1 {
            (*merged.pop().unwrap()).clone()
        } else {
            SymBytes::Concat(merged)
        }
    }
}
