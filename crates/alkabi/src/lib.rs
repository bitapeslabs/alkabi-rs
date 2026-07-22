//! alkabi — self-describing ABI generation for alkanes contracts.
//!
//! The [`AlkabiMessage`] derive implements `alkanes_runtime::message::MessageDispatch`
//! for a contract's message enum, generating:
//!   - `from_opcode`: calldata decoding (legacy u128-word fields or borsh-packed params)
//!   - `dispatch`: opcode -> snake_case method routing
//!   - `export_abi`: an alkabi JSON document exposed on-chain through `__meta`
//!     (wired by the upstream `declare_alkane!` macro)
//!
//! The [`AlkabiType`] derive describes borsh structs/enums as schema trees that
//! serialize into the borsh-js schema grammar consumed by borsher on the TS side.

pub mod abi;
pub mod abi_return;
#[cfg(feature = "extract")]
pub mod analysis;
pub mod borsh_io;
#[cfg(feature = "extract")]
pub mod extract;
pub mod legacy;
pub mod plan;
pub mod schema;
pub mod types;
pub mod witness;

pub use abi::AbiDocument as AlkabiAbi;
pub use abi_return::AlkabiResponse;
pub use alkabi_macros::{AlkabiMessage, AlkabiType};
pub use types::AlkabiType;
