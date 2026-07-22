use alkabi::{AlkabiMessage, AlkabiResponse, AlkabiType};
use alkanes_runtime::message::MessageDispatch;
use alkanes_runtime::runtime::AlkaneResponder;
use alkanes_support::id::AlkaneId;
use anyhow::Result;
use borsh::{BorshDeserialize, BorshSerialize};

#[derive(BorshSerialize, BorshDeserialize, AlkabiType, Debug, PartialEq)]
struct SchemaTestId {
    block: u32,
    tx: u64,
}

#[derive(BorshSerialize, BorshDeserialize, AlkabiType, Debug, PartialEq)]
struct SchemaBetParams {
    nonce: u128,
    target: u128,
    id: SchemaTestId,
    tags: Vec<String>,
    maybe: Option<u8>,
}

#[derive(BorshSerialize, BorshDeserialize, AlkabiType, Debug, PartialEq)]
struct SchemaBetResponse {
    won: u128,
}

#[derive(AlkabiType, Debug, PartialEq)]
#[allow(dead_code)]
enum UpgradeKind {
    Taquero,
    SalsaBar,
}

#[derive(Default)]
pub struct TestContract(());

impl AlkaneResponder for TestContract {}

// Handlers are fully typed: values or AlkabiResponse<T> envelopes, never a
// raw CallResponse. The bodies are unreachable stubs — the derive's dispatch
// only needs the signatures to type-check against the declarations.
impl TestContract {
    fn initialize(&self) -> Result<()> {
        Err(anyhow::anyhow!("not under test"))
    }
    fn get_name(&self) -> Result<String> {
        Err(anyhow::anyhow!("not under test"))
    }
    fn add_liquidity(&self, _token_a: AlkaneId, _amount_a: u128) -> Result<AlkabiResponse<()>> {
        Err(anyhow::anyhow!("not under test"))
    }
    fn set_label(&self, _label: String) -> Result<()> {
        Err(anyhow::anyhow!("not under test"))
    }
    fn swap(&self, _path: Vec<AlkaneId>, _amount_in: u128) -> Result<AlkabiResponse<()>> {
        Err(anyhow::anyhow!("not under test"))
    }
    fn bet(&self, _params: &SchemaBetParams) -> Result<AlkabiResponse<SchemaBetResponse>> {
        Err(anyhow::anyhow!("not under test"))
    }
    fn get_reserves(&self) -> Result<(u128, u128)> {
        Err(anyhow::anyhow!("not under test"))
    }
}

#[derive(AlkabiMessage, Debug)]
#[alkabi(contract = TestContract)]
enum TestMessage {
    #[opcode(0)]
    Initialize,

    #[opcode(99)]
    #[view]
    #[returns(String)]
    GetName,

    #[opcode(11)]
    AddLiquidity { token_a: AlkaneId, amount_a: u128 },

    #[opcode(12)]
    SetLabel { label: String },

    #[opcode(13)]
    Swap {
        path: Vec<AlkaneId>,
        amount_in: u128,
    },

    #[opcode(117)]
    #[borsh]
    #[returns(borsh(SchemaBetResponse))]
    Bet(SchemaBetParams),

    #[opcode(97)]
    #[view]
    #[returns(u128, u128)]
    GetReserves,
}

fn parse(opcode: u128, inputs: Vec<u128>) -> TestMessage {
    <TestMessage as MessageDispatch<TestContract>>::from_opcode(opcode, inputs).unwrap()
}

#[test]
fn parses_unit_variant() {
    assert!(matches!(parse(0, vec![]), TestMessage::Initialize));
}

#[test]
fn parses_legacy_fields() {
    let msg = parse(11, vec![2, 7, 1000]);
    match msg {
        TestMessage::AddLiquidity { token_a, amount_a } => {
            assert_eq!(token_a, AlkaneId::new(2, 7));
            assert_eq!(amount_a, 1000);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parses_legacy_string_nul_terminated() {
    // "HI" followed by a NUL terminator, packed little-endian into one word.
    let word = u128::from_le_bytes(*b"HI\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
    match parse(12, vec![word]) {
        TestMessage::SetLabel { label } => assert_eq!(label, "HI"),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parses_legacy_vec_of_alkane_ids() {
    // len=2, then two (block, tx) pairs, then amount_in.
    let msg = parse(13, vec![2, 1, 10, 1, 11, 5000]);
    match msg {
        TestMessage::Swap { path, amount_in } => {
            assert_eq!(path, vec![AlkaneId::new(1, 10), AlkaneId::new(1, 11)]);
            assert_eq!(amount_in, 5000);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parses_borsh_params_with_word_padding() {
    let params = SchemaBetParams {
        nonce: 42,
        target: 7,
        id: SchemaTestId { block: 2, tx: 1039 },
        tags: vec!["a".to_string(), "bc".to_string()],
        maybe: Some(9),
    };
    let bytes = borsh::to_vec(&params).unwrap();

    // Pack into u128 words the way the TS client does: 16-byte LE chunks,
    // final word zero-padded.
    let mut words = Vec::new();
    for chunk in bytes.chunks(16) {
        let mut buf = [0u8; 16];
        buf[..chunk.len()].copy_from_slice(chunk);
        words.push(u128::from_le_bytes(buf));
    }

    match parse(117, words) {
        TestMessage::Bet(decoded) => assert_eq!(decoded, params),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn rejects_unknown_opcode() {
    let err = <TestMessage as MessageDispatch<TestContract>>::from_opcode(9999, vec![])
        .unwrap_err()
        .to_string();
    assert!(err.contains("Unknown opcode"), "{}", err);
}

#[test]
fn export_abi_document_shape() {
    let json = String::from_utf8(
        <TestMessage as MessageDispatch<TestContract>>::export_abi(),
    )
    .unwrap();
    let expected = concat!(
        "{\"alkabi\":1,\"contract\":\"TestContract\",",
        "\"types\":{",
        "\"AlkaneId\":{\"struct\":{\"block\":\"u128\",\"tx\":\"u128\"}},",
        "\"SchemaBetParams\":{\"struct\":{\"nonce\":\"u128\",\"target\":\"u128\",\"id\":{\"$ref\":\"SchemaTestId\"},\"tags\":{\"array\":{\"type\":\"string\"}},\"maybe\":{\"option\":\"u8\"}}},",
        "\"SchemaBetResponse\":{\"struct\":{\"won\":\"u128\"}},",
        "\"SchemaTestId\":{\"struct\":{\"block\":\"u32\",\"tx\":\"u64\"}}",
        "},",
        "\"methods\":[",
        "{\"name\":\"initialize\",\"opcode\":0,\"kind\":\"execute\"},",
        "{\"name\":\"getName\",\"opcode\":99,\"kind\":\"view\",\"output\":{\"mode\":\"raw\",\"schema\":\"string\"}},",
        "{\"name\":\"addLiquidity\",\"opcode\":11,\"kind\":\"execute\",\"input\":{\"mode\":\"legacy\",\"schema\":{\"struct\":{\"token_a\":{\"$ref\":\"AlkaneId\"},\"amount_a\":\"u128\"}}}},",
        "{\"name\":\"setLabel\",\"opcode\":12,\"kind\":\"execute\",\"input\":{\"mode\":\"legacy\",\"schema\":{\"struct\":{\"label\":\"string\"}}}},",
        "{\"name\":\"swap\",\"opcode\":13,\"kind\":\"execute\",\"input\":{\"mode\":\"legacy\",\"schema\":{\"struct\":{\"path\":{\"array\":{\"type\":{\"$ref\":\"AlkaneId\"}}},\"amount_in\":\"u128\"}}}},",
        "{\"name\":\"bet\",\"opcode\":117,\"kind\":\"execute\",\"input\":{\"mode\":\"borsh\",\"schema\":{\"$ref\":\"SchemaBetParams\"}},\"output\":{\"mode\":\"borsh\",\"schema\":{\"$ref\":\"SchemaBetResponse\"}}},",
        "{\"name\":\"getReserves\",\"opcode\":97,\"kind\":\"view\",\"output\":{\"mode\":\"raw\",\"schema\":{\"struct\":{\"_0\":\"u128\",\"_1\":\"u128\"}}}}",
        "]}",
    );
    assert_eq!(json, expected);
}

// The `extract` feature is enabled whenever the whole workspace builds (the
// CLI requires it); `cargo test -p alkabi` without the feature skips these.
#[cfg(feature = "extract")]
mod extract_tests {
    use super::*;
    use alkabi::abi::MethodKind;
    use alkabi::extract::parse_abi_json;
    use alkabi::schema::Schema;

    #[test]
    fn parse_roundtrips_native_documents() {
        let json = String::from_utf8(
            <TestMessage as MessageDispatch<TestContract>>::export_abi(),
        )
        .unwrap();
        let (abi, normalized) = parse_abi_json(&json).unwrap();
        assert!(!normalized);
        assert_eq!(abi.contract, "TestContract");
        assert_eq!(abi.methods.len(), 7);
        assert_eq!(abi.to_json(), json);
    }

    #[test]
    fn normalizes_upstream_documents() {
        let upstream = concat!(
            "{ \"contract\": \"AMMFactory\", \"methods\": [",
            "{ \"name\": \"init_factory\", \"opcode\": 0, \"params\": [",
            "{\"type\": \"u128\", \"name\": \"pool_factory_id\"},",
            "{\"type\": \"AlkaneId\", \"name\": \"beacon_id\"}], \"returns\": \"void\" },",
            "{ \"name\": \"get_reserves\", \"opcode\": 97, \"params\": [], \"returns\": \"u128, u128\" },",
            "{ \"name\": \"swap_exact\", \"opcode\": 13, \"params\": [",
            "{\"type\": \"Vec<AlkaneId>\", \"name\": \"path\"}], \"returns\": \"void\" }",
            "] }",
        );
        let (abi, normalized) = parse_abi_json(upstream).unwrap();
        assert!(normalized);
        assert_eq!(abi.contract, "AMMFactory");
        assert!(abi.types.get("AlkaneId").is_some());

        assert_eq!(abi.methods[0].name, "initFactory");
        assert_eq!(abi.methods[0].kind, MethodKind::Execute);
        assert_eq!(abi.methods[0].opcode, 0);
        let input = abi.methods[0].input.as_ref().unwrap();
        assert_eq!(
            input.schema,
            Schema::Struct(vec![
                ("pool_factory_id".to_string(), Schema::Primitive("u128")),
                ("beacon_id".to_string(), Schema::Ref("AlkaneId".to_string())),
            ])
        );

        assert_eq!(abi.methods[1].name, "getReserves");
        assert_eq!(abi.methods[1].kind, MethodKind::View);
        assert_eq!(
            abi.methods[1].output.as_ref().unwrap().schema,
            Schema::Struct(vec![
                ("_0".to_string(), Schema::Primitive("u128")),
                ("_1".to_string(), Schema::Primitive("u128")),
            ])
        );

        assert_eq!(
            abi.methods[2].input.as_ref().unwrap().schema,
            Schema::Struct(vec![(
                "path".to_string(),
                Schema::Vec(Box::new(Schema::Ref("AlkaneId".to_string()))),
            )])
        );
    }
}

#[test]
fn enum_schema_shape() {
    use alkabi::schema::Schema;
    let schema = <UpgradeKind as alkabi::AlkabiType>::schema();
    assert_eq!(
        schema,
        Schema::Enum(vec![
            ("Taquero".to_string(), Schema::Struct(vec![])),
            ("SalsaBar".to_string(), Schema::Struct(vec![])),
        ])
    );
    assert_eq!(
        schema.to_json(),
        "{\"enum\":[{\"struct\":{\"Taquero\":{\"struct\":{}}}},{\"struct\":{\"SalsaBar\":{\"struct\":{}}}}]}"
    );
}
