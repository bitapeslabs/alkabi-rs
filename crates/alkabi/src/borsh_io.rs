//! Borsh calldata decoding: the words after the opcode are flattened into
//! little-endian bytes and borsh-deserialized. The final word may carry zero
//! padding, so decoding must tolerate trailing bytes (`deserialize_reader`,
//! not `try_from_slice`). This mirrors tacoclicker's `decode_from_ctx!`.

use anyhow::{anyhow, Result};
use borsh::BorshDeserialize;
use std::io::Cursor;

pub fn words_to_bytes(words: &[u128]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

pub fn decode_words<T: BorshDeserialize>(words: &[u128]) -> Result<T> {
    let bytes = words_to_bytes(words);
    let mut cursor = Cursor::new(bytes);
    T::deserialize_reader(&mut cursor).map_err(|e| anyhow!("Failed to borsh-decode params: {}", e))
}
