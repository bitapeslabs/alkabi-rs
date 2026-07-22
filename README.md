# alkabi-rs

Self-describing ABIs for [alkanes](https://github.com/kungfuflex/alkanes-rs) smart contracts.

Declare your contract's message enum once. alkabi derives the dispatch, decodes
calldata and witness payloads into typed structs before your handlers run,
enforces your declared return types at compile time, and embeds a
self-describing `abi.json` in the compiled wasm (served on-chain through the
standard `__meta` export). The same ABI can be extracted from **any** alkanes
contract wasm — including pre-alkabi contracts, which are normalized into the
alkabi format.

The ABI schemas use the borsh-js grammar that
[borsher](https://github.com/nameless-il/borsher) wraps, so a TypeScript
consumer can rebuild runtime codecs and static types from `abi.json` with zero
codegen. See [DESIGN.md](DESIGN.md) for the full format specification.

## Using alkabi in a contract

```toml
[dependencies]
alkabi = { git = "https://github.com/bitapeslabs/alkabi-rs" }
alkanes-runtime = { git = "https://github.com/kungfuflex/alkanes-rs", tag = "v2.2.1-alpha.1" }
alkanes-support = { git = "https://github.com/kungfuflex/alkanes-rs", tag = "v2.2.1-alpha.1" }
metashrew-support = { git = "https://github.com/sandshrewmetaprotocols/metashrew", tag = "v9.0.5-rc.8" }
borsh = { version = "1.5", features = ["derive"] }
anyhow = "1"
```

> **Pins matter.** alkabi builds against `alkanes-rs` tag `v2.2.1-alpha.1`;
> your contract must use the same source + tag so cargo unifies on one copy of
> `alkanes-support` (one `AlkaneId` type). Building `metashrew-support`
> requires a `protoc` binary on PATH (`apt install protobuf-compiler`).

### Declaring the ABI

Describe your borsh schemas with one extra derive, and your opcodes on the
message enum:

```rust
use alkabi::{AlkabiMessage, AlkabiResponse, AlkabiType};
use borsh::{BorshDeserialize, BorshSerialize};

#[derive(BorshSerialize, BorshDeserialize, AlkabiType)]
pub struct SchemaBetParams {
    pub nonce: u128,
    pub target_multiplier: u128,
}

#[derive(BorshSerialize, BorshDeserialize, AlkabiType)]
pub struct SchemaBetResponse {
    pub won_amount: u128,
}

#[derive(AlkabiMessage)]
enum MyContractMessage {
    #[opcode(0)]
    Initialize,

    // Legacy calldata: positional u128 words (u128 = 1 word, AlkaneId = 2,
    // String = NUL-terminated packed, Vec<T> = length-prefixed).
    #[opcode(11)]
    AddLiquidity { token_a: AlkaneId, amount_a: u128 },

    // Read-only (simulate) method with a raw-encoded return.
    #[opcode(99)]
    #[view]
    #[returns(String)]
    GetName,

    // Borsh calldata in, borsh out. The handler receives the decoded struct.
    #[opcode(117)]
    #[borsh]
    #[returns(borsh(SchemaBetResponse))]
    Bet(SchemaBetParams),

    // Borsh payload carried in the reveal transaction's witness envelope.
    // alkabi fetches the tx, finds the envelope, and decodes it for you.
    #[opcode(121)]
    #[witness(SchemaBetParams)]
    ClaimSomething,
}
```

Variant attributes:

| Attribute | Meaning |
|---|---|
| `#[opcode(n)]` | required; the u128 opcode |
| `#[view]` | read-only → simulate; default is execute |
| `#[borsh]` | the single variant field is a borsh params struct decoded from calldata |
| `#[witness(T)]` | borsh payload in the witness envelope, passed as a trailing `&T` |
| `#[returns(T)]` | raw-mode return (LE ints, bare UTF-8 strings, `Vec<u8>`, int tuples) |
| `#[returns(borsh(T))]` | borsh-encoded return |
| `#[alkabi(contract = X)]` | enum-level; responder type (default strips a `Message` suffix) |

### Writing handlers

Handlers are typed end to end. Inputs arrive decoded; returns are checked at
compile time against the declaration and encoded by alkabi — `CallResponse`
and its untyped `Vec<u8>` data never appear in your signatures:

```rust
impl MyContract {
    // Value-only handlers just return the declared type; alkabi encodes it
    // and forwards the incoming alkanes.
    fn get_name(&self) -> Result<String> {
        Ok("MYTOKEN".to_string())
    }

    // Handlers that move alkane transfers return the typed response envelope.
    fn bet(&self, params: &SchemaBetParams) -> Result<AlkabiResponse<SchemaBetResponse>> {
        let ctx = self.context()?;
        let mut response = AlkabiResponse::forward(&ctx.incoming_alkanes);
        response.alkanes.0.push(/* ... */);
        Ok(response.with_data(SchemaBetResponse { won_amount: 42 }))
    }

    // Void methods are typed as `()`.
    fn initialize(&self) -> Result<AlkabiResponse<()>> { /* ... */ }
    fn add_liquidity(&self, token_a: AlkaneId, amount_a: u128) -> Result<()> { /* ... */ }

    // Witness payloads arrive as the trailing argument.
    fn claim_something(&self, params: &SchemaBetParams) -> Result<()> { /* ... */ }
}
```

Returning the wrong type is a compile error naming the declaration:

```
error[E0277]: the trait bound `u32: AbiReturnShape<_, RawMode, u64>` is not satisfied
```

### Wiring it up

alkabi implements the standard `MessageDispatch` trait, so the upstream
`declare_alkane!` macro works unchanged (it also wires `export_abi()` to the
`__meta` wasm export — your deployed contract self-reports its ABI on-chain):

```rust
use alkanes_runtime::{declare_alkane, message::MessageDispatch, runtime::AlkaneResponder};
use metashrew_support::compat::to_arraybuffer_layout;

impl AlkaneResponder for MyContract {}

declare_alkane! {
    impl AlkaneResponder for MyContract {
        type Message = MyContractMessage;
    }
}
```

Build as usual:

```sh
cargo build --release --target wasm32-unknown-unknown
```

## Extracting ABIs from wasm (library)

Enable the `extract` feature (host-side only — never in contract builds) to
turn compiled wasm bytes into an `AlkabiAbi`:

```toml
[dependencies]
alkabi = { git = "https://github.com/bitapeslabs/alkabi-rs", features = ["extract"] }
```

```rust
let wasm = std::fs::read("contract.wasm")?;
let abi: alkabi::AlkabiAbi = alkabi::extract::extract_abi(&wasm)?;

println!("{}", abi.contract);
for method in &abi.methods {
    println!("{} (opcode {})", method.name, method.opcode);
}
println!("{}", abi.to_json_pretty());
```

This works on any alkanes contract with a `__meta` export. Pre-alkabi
contracts (built with the upstream `MessageDispatch` derive) are normalized
into the alkabi format; their view/execute kinds are inferred with a `get_*`
heuristic, so verify those by hand. `alkabi::extract::parse_abi_json` accepts
ABI JSON directly (e.g. from the indexer's `meta` view) and reports whether
normalization happened.

## The CLI

```sh
cargo install --git https://github.com/bitapeslabs/alkabi-rs alkabi-extract

alkabi-extract path/to/contract.wasm            # writes ./abis/abi.json + abi.ts
alkabi-extract path/to/contract.wasm -o my-dir  # custom output directory
```

`abi.json` is the pretty-printed document; `abi.ts` is the same object as an
`export const ... as const` — the literal-typed form a TypeScript consumer
imports to get full static types without codegen.
