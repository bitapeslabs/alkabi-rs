//! The `AlkabiType` trait: a Rust type's schema description.

use crate::schema::{Schema, TypeRegistry};
use alkanes_support::id::AlkaneId;

/// Describes a type as a schema tree.
///
/// Named types (derived structs/enums, `AlkaneId`) set `NAME`, appear in the ABI
/// document's `types` section, and are referenced via `$ref` from other schemas.
/// Primitives and containers inline.
pub trait AlkabiType {
    const NAME: Option<&'static str> = None;

    /// The full schema definition. Fields of named types are referenced, not inlined.
    fn schema() -> Schema;

    /// How other schemas point at this type: `$ref` for named types, inline otherwise.
    fn reference() -> Schema {
        match Self::NAME {
            Some(name) => Schema::Ref(name.to_string()),
            None => Self::schema(),
        }
    }

    /// Register this type (and, transitively, every named type it uses) in `reg`.
    fn collect(reg: &mut TypeRegistry) {
        let _ = reg;
    }
}

macro_rules! impl_primitive {
    ($($ty:ty => $name:literal),* $(,)?) => {
        $(
            impl AlkabiType for $ty {
                fn schema() -> Schema {
                    Schema::Primitive($name)
                }
            }
        )*
    };
}

impl_primitive! {
    u8 => "u8",
    u16 => "u16",
    u32 => "u32",
    u64 => "u64",
    u128 => "u128",
    i8 => "i8",
    i16 => "i16",
    i32 => "i32",
    i64 => "i64",
    i128 => "i128",
    f32 => "f32",
    f64 => "f64",
    bool => "bool",
    String => "string",
}

impl<T: AlkabiType> AlkabiType for Vec<T> {
    fn schema() -> Schema {
        Schema::Vec(Box::new(T::reference()))
    }

    fn collect(reg: &mut TypeRegistry) {
        T::collect(reg);
    }
}

impl<T: AlkabiType> AlkabiType for Option<T> {
    fn schema() -> Schema {
        Schema::Option(Box::new(T::reference()))
    }

    fn collect(reg: &mut TypeRegistry) {
        T::collect(reg);
    }
}

impl<T: AlkabiType, const N: usize> AlkabiType for [T; N] {
    fn schema() -> Schema {
        Schema::Array(Box::new(T::reference()), N)
    }

    fn collect(reg: &mut TypeRegistry) {
        T::collect(reg);
    }
}

impl AlkabiType for AlkaneId {
    const NAME: Option<&'static str> = Some("AlkaneId");

    fn schema() -> Schema {
        Schema::Struct(vec![
            ("block".to_string(), Schema::Primitive("u128")),
            ("tx".to_string(), Schema::Primitive("u128")),
        ])
    }

    fn collect(reg: &mut TypeRegistry) {
        if !reg.contains("AlkaneId") {
            reg.insert("AlkaneId", Self::schema());
        }
    }
}
