mod abitype;
mod message;
mod util;

use proc_macro::TokenStream;
use syn::{parse_macro_input, DeriveInput};

/// Derive `MessageDispatch` (dispatch + calldata decoding + alkabi ABI export)
/// for a contract message enum.
///
/// Variant attributes:
///   - `#[opcode(n)]` (required): the u128 opcode.
///   - `#[view]`: marks the method read-only (simulate); default is execute.
///   - `#[borsh]`: the variant carries exactly one field, a borsh params struct
///     decoded from the word-packed calldata. The handler receives it by reference.
///   - `#[witness(T)]`: a borsh payload carried in the reveal transaction's
///     witness envelope. alkabi fetches and decodes it; the handler receives
///     it as a trailing `&T` argument.
///   - `#[returns(T)]` / `#[returns(T, U)]`: raw-mode return (LE integers,
///     bare UTF-8 strings, `Vec<u8>` bytes; tuples become `_0`/`_1` structs).
///   - `#[returns(borsh(T))]`: the response data is a borsh-serialized `T`.
///
/// Handler return types are enforced at compile time against the declaration:
/// a handler returns `Result<T>` (alkabi encodes it and forwards incoming
/// alkanes) or `Result<AlkabiResponse<T>>` (typed data plus explicit alkane
/// transfer control). Void methods use `T = ()`. Raw `CallResponse` returns do
/// not compile — alkabi owns the conversion to the `Vec<u8>` data that
/// `MessageDispatch` expects.
///
/// Enum attribute:
///   - `#[alkabi(contract = Type)]`: the responder type. Defaults to the enum
///     name with a trailing `Message` stripped.
#[proc_macro_derive(AlkabiMessage, attributes(opcode, view, borsh, returns, alkabi, witness))]
pub fn derive_alkabi_message(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    message::expand(input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// Derive `AlkabiType` for a borsh schema struct or enum, describing it as a
/// schema tree that serializes to the borsh-js grammar.
#[proc_macro_derive(AlkabiType)]
pub fn derive_alkabi_type(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    abitype::expand(input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}
