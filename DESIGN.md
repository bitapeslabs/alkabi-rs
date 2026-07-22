# alkabi — self-describing ABIs for alkanes contracts

## Goal

Kill the hand-maintained mirroring between alkanes contracts (Rust) and their
TypeScript clients. The contract's message enum is the single source of truth;
compiling it yields an `abi.json` / `abi.ts` pair that the TS side (alkanesjs +
borsher) consumes to derive both runtime encoders and static types — no logic
codegen, abitype-style.

## Architecture

```
contract crate
  #[derive(AlkabiMessage)] on the message enum
  #[derive(AlkabiType)] on borsh schema structs/enums
        │  implements alkanes_runtime::message::MessageDispatch
        │  (from_opcode / dispatch / export_abi)
        ▼
declare_alkane! (upstream, unchanged) wires export_abi() → __meta wasm export
        │
        ▼  cargo build --release --target wasm32-unknown-unknown
contract.wasm ── alkabi-extract ──► abis/abi.json + abis/abi.ts
                 (instantiates the wasm with all imports stubbed,
                  calls __meta, reads the length-prefixed buffer)
```

Because `__meta` is a real on-chain export, the deployed contract serves the
exact same document through the indexer's `meta` view — chain-pull tooling can
regenerate `abi.ts` from a live alkane id.

## Crates

- `crates/alkabi` — runtime support: `Schema` tree + JSON writer (hand-rolled,
  no serde in contract wasm), `AbiDocument`, the `AlkabiType` trait,
  `legacy::{LegacyReader, LegacyDecode}` (upstream-compatible word decoding),
  `borsh_io::decode_words` (word-packed borsh calldata).
- `crates/alkabi-macros` — `#[derive(AlkabiMessage)]`, `#[derive(AlkabiType)]`.
- `crates/alkabi` (feature `extract`) — the host-side **library API**: pass
  compiled wasm bytes, get an `AlkabiAbi` struct back.

  ```rust
  // alkabi = { version = "...", features = ["extract"] }
  let abi: alkabi::AlkabiAbi = alkabi::extract::extract_abi(&wasm_bytes)?;
  println!("{}", abi.to_json());        // or abi.to_json_pretty()
  ```

  `AlkabiAbi` (= `abi::AbiDocument`, the same struct `export_abi()` builds) has
  public `contract` / `types` / `methods` fields for programmatic inspection.
  Lower-level pieces are exposed too: `extract::extract_meta_bytes` (run
  `__meta` in wasmi with all imports stubbed) and `extract::parse_abi_json`
  (parse + normalize, returns a `normalized` flag). Feature-gated so contract
  builds never pull wasmi/serde_json (`preserve_order` since borsh struct field
  order is wire-significant; `arbitrary_precision` so u128 opcodes survive
  round-trips).

  **Normalization** is part of the library: contracts built with the upstream
  `MessageDispatch` derive (any alkanes contract with a `__meta` export) have
  their primitive ABI upconverted to an alkabi v1 document — snake_case →
  camelCase, `params` → legacy-mode struct schemas, `returns` → raw-mode
  schemas (comma tuples → `_0`/`_1` structs). `kind` is a heuristic there
  (`get_*` → view, else execute) since the upstream enum never recorded it —
  hand-verify normalized outputs. See `alkabi-contracts/dumps/` for examples
  extracted from the oyl-amm production wasms.
- `crates/alkabi-extract` — thin CLI over the library: wasm →
  `abis/abi.json` + `abis/abi.ts`.

## Derive reference

```rust
#[derive(AlkabiMessage)]
#[alkabi(contract = MyContract)]        // optional; default strips "Message"
enum MyContractMessage {
    #[opcode(0)]
    #[returns(String)]                  // raw-mode return
    Initialize,

    #[opcode(11)]                       // legacy params: positional u128 words
    AddLiquidity { token_a: AlkaneId, amount_a: u128 },

    #[opcode(97)]
    #[view]                             // read-only → simulate; default execute
    #[returns(u128, u128)]              // tuple → raw struct {_0,_1}
    GetReserves,

    #[opcode(117)]
    #[borsh]                            // single borsh params struct,
    #[returns(borsh(SchemaBetResponse))]// handler receives it by reference:
    Bet(SchemaBetParams),               // fn bet(&self, p: &SchemaBetParams)

    #[opcode(121)]
    #[witness(SchemaMerkleProof)]       // borsh payload in the reveal-tx
    ClaimAirdrop,                       // witness envelope; alkabi fetches +
                                        // decodes it, handler gets trailing &T:
                                        // fn claim_airdrop(&self, p: &SchemaMerkleProof)
}
```

**Typed returns, enforced at compile time.** Response data is typed as the
declaration; `CallResponse` (untyped `Vec<u8>` data) never appears in handler
signatures — alkabi alone converts to the bytes `MessageDispatch` expects.
Handlers return exactly one of two shapes, enforced by trait bounds in the
generated dispatch (`alkabi::abi_return`):

- `Result<T>` where `T` is the declared type — alkabi encodes it (raw LE /
  borsh per the declaration) and forwards incoming alkanes. Returning the
  wrong type is a compile error
  (``the trait bound `u32: AbiReturnShape<_, RawMode, u64>` is not satisfied``).
- `Result<AlkabiResponse<T>>` — the typed-data response envelope
  (`{ alkanes: AlkaneTransferParcel, data: T }`) for handlers that also move
  alkane transfers. Build with `AlkabiResponse::forward(&incoming)` (data `()`)
  and attach the value with `.with_data(value)`.

Void methods are `T = ()` (encodes to zero bytes): transfer-only handlers
return `Result<()>` or `Result<AlkabiResponse<()>>`. Returning a raw
`CallResponse` does not compile.

**Witness inputs.** `#[witness(T)]` declares a borsh payload carried in the
reveal transaction's witness envelope instead of calldata (for data too large
for calldata). The generated dispatch calls `alkabi::witness::decode_witness`,
which fetches the transaction from the host, finds the envelope payload in any
input (via `alkanes_support::witness::find_witness_payload`), and borsh-decodes
it with descriptive errors — every alkabi contract reads witness data the same
way. Composable with calldata params: the witness value is always the
handler's trailing argument.

Legacy decoding replicates upstream alkanes-macros (rev `5b828be9`) exactly:
`u128` = 1 word, `AlkaneId` = 2 words, `String` = NUL-terminated LE-packed
bytes, `Vec<T>` = length word + elements. Borsh decoding flattens the words to
LE bytes and uses `deserialize_reader` (tolerates the zero padding in the final
word) — identical to tacoclicker's `decode_from_ctx!`.

`#[derive(AlkabiType)]` on borsh structs/enums records their schema; named
types land in the document's `types` section and are referenced via `$ref`.

## ABI document format (alkabi v1)

```jsonc
{
  "alkabi": 1,
  "contract": "Tortilla",
  "types": {                       // named schemas, $ref targets, sorted
    "AlkaneId": { "struct": { "block": "u128", "tx": "u128" } }
  },
  "methods": [
    {
      "name": "betOnBlock",        // camelCase, as the TS client exposes it
      "opcode": 117,               // bare JSON number (u128; JS consumers of
                                   // opcodes above 2^53 must avoid JSON.parse)
      "kind": "execute",           // "view" | "execute"
      "input":  { "mode": "borsh",  "schema": { "$ref": "SchemaBetParams" } },
      "output": { "mode": "borsh",  "schema": { "$ref": "SchemaBetResponse" } }
    },
    {
      "name": "claimAirdrop",
      "opcode": 121,
      "kind": "execute",           // witness payload rides in the reveal-tx
      "witness": { "mode": "borsh", "schema": { "$ref": "SchemaMerkleProof" } }
    }
  ]
}
```

Schemas use the borsh-js grammar borsher wraps — primitives as bare strings
(`"u128"`, `"string"`), `{"struct":{...}}` (field order significant),
`{"option":...}`, `{"array":{"type":...}}` (+`"len"` for fixed arrays),
`{"enum":[{"struct":{"Variant":...}}]}` — plus the `{"$ref":"Name"}` extension.

IO modes tell the client how bytes relate to the schema:

- `legacy` (inputs): positional u128 words after the opcode. Note borsh
  serialization of fixed-width fields chunked into 16-byte LE words *is* the
  legacy encoding; only `Vec` length width (u128 word vs borsh u32) and
  `String` packing (NUL-terminated vs length-prefixed) diverge.
- `borsh`: borsh bytes — word-chunked for inputs, raw `CallResponse.data` for
  outputs.
- `raw` (outputs): fixed-width LE integers, bare UTF-8 strings to end of
  buffer, `Vec<u8>` = remaining bytes, tuples = concatenated LE fields.

Omitted `input`/`output` mean no calldata / void. `witness` (always mode
`borsh`) describes a payload in the reveal transaction's witness envelope; it
composes with `input` — a method may take calldata and witness data at once.

## Version pins

Espo-style GitHub tag pins, no patches or vendoring:

- `alkanes-rs` tag `v2.2.1-alpha.1` (same tag espo uses). The `MessageDispatch`
  trait includes `export_abi` and `declare_alkane!` wires `__meta` at this tag.
  It pins metashrew by tag internally, so the whole graph resolves
  deterministically.
- `metashrew-support` tag `v9.0.5-rc.8` in alkabi-contracts (must mirror what
  alkanes-rs pins so cargo unifies on one copy).

Contracts adopting alkabi must use the same alkanes-rs source/tag alkabi pins,
otherwise cargo builds two copies of alkanes-support and the `AlkaneId` types
won't unify.

Build prerequisite: a `protoc` binary (modern metashrew-support generates
protobuf code via prost). `apt install protobuf-compiler`, or set `PROTOC` to a
downloaded binary — the same prerequisite espo and tacoclicker builds have.

## Flow

```sh
cd alkabi-contracts
cargo build --release --target wasm32-unknown-unknown
cargo run --manifest-path ../alkabi/Cargo.toml -p alkabi-extract -- \
    target/wasm32-unknown-unknown/release/clock_in.wasm -o clock-in/abis
```

## Not yet built (next phases)

- `alkabi-ts`: loader turning `abi.ts` into alkanesjs `ViewSpec`/`ExecuteSpec`
  records via `abi.attach` (witness schemas map onto alkanesjs's existing
  inscription argument), runtime borsher schema reconstruction from the JSON
  grammar, a legacy-mode encoder walker, and the type-level `InferFromJson`
  interpreter.
- Chain-pull: regenerate `abi.ts` from a deployed alkane via the `meta` view.
- `u256` / bespoke raw layouts (oyl `PoolInfo`-style) escape hatches.
- Protostone-shape validation as an opt-in for witness methods (tacoclicker's
  `validate_protostone_tx` stays contract-side for now).
