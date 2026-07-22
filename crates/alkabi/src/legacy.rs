//! Legacy calldata decoding: positional u128 words after the opcode.
//!
//! Semantics replicate the upstream `alkanes-macros` MessageDispatch derive
//! (rev 5b828be9) exactly:
//!   - `u128`: one word
//!   - `AlkaneId`: two words (block, tx)
//!   - `String`: consume words, scanning each word's little-endian bytes until a
//!     NUL byte; bytes after the NUL in that word are discarded
//!   - `Vec<T>`: one length word, then `length` elements decoded recursively

use alkanes_support::id::AlkaneId;
use anyhow::{anyhow, Result};

pub struct LegacyReader<'a> {
    inputs: &'a [u128],
    pos: usize,
}

impl<'a> LegacyReader<'a> {
    pub fn new(inputs: &'a [u128]) -> Self {
        Self { inputs, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.inputs.len() - self.pos
    }

    pub fn next(&mut self) -> Option<u128> {
        let value = self.inputs.get(self.pos).copied();
        if value.is_some() {
            self.pos += 1;
        }
        value
    }
}

pub trait LegacyDecode: Sized {
    fn decode(reader: &mut LegacyReader) -> Result<Self>;
}

impl LegacyDecode for u128 {
    fn decode(reader: &mut LegacyReader) -> Result<Self> {
        reader.next().ok_or_else(|| anyhow!("Missing u128 parameter"))
    }
}

impl LegacyDecode for AlkaneId {
    fn decode(reader: &mut LegacyReader) -> Result<Self> {
        if reader.remaining() < 2 {
            return Err(anyhow!("Not enough parameters provided for AlkaneId"));
        }
        let block = reader.next().unwrap();
        let tx = reader.next().unwrap();
        Ok(AlkaneId::new(block, tx))
    }
}

impl LegacyDecode for String {
    fn decode(reader: &mut LegacyReader) -> Result<Self> {
        if reader.remaining() == 0 {
            return Err(anyhow!("Not enough parameters provided for string"));
        }
        let mut string_bytes = Vec::new();
        let mut found_null = false;
        while !found_null {
            let Some(word) = reader.next() else {
                break;
            };
            for byte in word.to_le_bytes() {
                if byte == 0 {
                    found_null = true;
                    break;
                }
                string_bytes.push(byte);
            }
        }
        String::from_utf8(string_bytes).map_err(|e| anyhow!("Invalid UTF-8 string: {}", e))
    }
}

impl<T: LegacyDecode> LegacyDecode for Vec<T> {
    fn decode(reader: &mut LegacyReader) -> Result<Self> {
        let length = reader
            .next()
            .ok_or_else(|| anyhow!("Missing length parameter for Vec"))? as usize;
        let mut vec = Vec::with_capacity(length.min(1024));
        for _ in 0..length {
            vec.push(T::decode(reader)?);
        }
        Ok(vec)
    }
}
