//! Witness-borne inputs.
//!
//! Some payloads are too large for calldata and ride in the reveal
//! transaction's witness instead (an ord-style envelope). A variant declaring
//! `#[witness(T)]` gets this handled entirely by alkabi: the generated dispatch
//! arm calls [`decode_witness`], which fetches the transaction from the host,
//! locates the envelope payload in any input, and borsh-decodes it — the
//! handler just receives `&T`. Every alkabi contract reads witness data the
//! same way.

use alkanes_runtime::runtime::AlkaneResponder;
use alkanes_support::witness::find_witness_payload;
use anyhow::{anyhow, Result};
use bitcoin::Transaction;
use borsh::BorshDeserialize;
use metashrew_support::utils::consensus_decode;
use std::io::Cursor;

/// Find the first non-empty envelope payload across all inputs. Ordinals
/// conventionally uses input 0, but looping covers the edge cases.
pub fn witness_payload(tx: &Transaction) -> Option<Vec<u8>> {
    (0..tx.input.len()).find_map(|idx| {
        find_witness_payload(tx, idx).filter(|payload| !payload.is_empty())
    })
}

/// Fetch the current transaction and borsh-decode its witness payload as `T`.
pub fn decode_witness<T, C>(responder: &C) -> Result<T>
where
    T: BorshDeserialize,
    C: AlkaneResponder,
{
    let tx = consensus_decode::<Transaction>(&mut Cursor::new(responder.transaction()))
        .map_err(|_| anyhow!("alkabi: failed to consensus-decode the transaction"))?;

    let payload = witness_payload(&tx)
        .ok_or_else(|| anyhow!("alkabi: no witness envelope payload found in any input"))?;

    let mut reader = Cursor::new(payload);
    T::deserialize_reader(&mut reader).map_err(|e| {
        anyhow!(
            "alkabi: failed to borsh-decode witness payload as {}: {}",
            core::any::type_name::<T>(),
            e
        )
    })
}
