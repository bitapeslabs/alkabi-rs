//! Schema trees mirroring the borsh-js schema grammar (the format borsher wraps),
//! extended with `$ref` nodes that point into the ABI document's `types` section.

use std::collections::BTreeMap;

/// A type schema. Serializes to the exact JSON grammar borsh-js accepts:
/// primitives as bare strings (`"u128"`), compounds as single-key objects
/// (`{"struct":{...}}`, `{"option":...}`, `{"array":{"type":...}}`,
/// `{"enum":[{"struct":{...}}]}`), plus the alkabi `{"$ref":"Name"}` extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schema {
    /// "u8".."u128", "i8".."i128", "f32", "f64", "bool", "string"
    Primitive(&'static str),
    /// Ordered named fields. Order is borsh-significant and preserved in JSON.
    Struct(Vec<(String, Schema)>),
    /// Ordered variants; borsh discriminant is the index (u8).
    Enum(Vec<(String, Schema)>),
    Option(Box<Schema>),
    Vec(Box<Schema>),
    Array(Box<Schema>, usize),
    /// Reference to a named entry in the document's `types` section.
    Ref(String),
}

impl Schema {
    /// Look up a primitive by its wire name ("u8".."u128", "i8".."i128",
    /// "f32", "f64", "bool", "string").
    pub fn primitive(name: &str) -> Option<Schema> {
        const PRIMITIVES: &[&str] = &[
            "u8", "u16", "u32", "u64", "u128", "i8", "i16", "i32", "i64", "i128", "f32", "f64",
            "bool", "string",
        ];
        PRIMITIVES
            .iter()
            .find(|p| **p == name)
            .map(|p| Schema::Primitive(p))
    }

    pub fn write_json(&self, out: &mut String) {
        match self {
            Schema::Primitive(p) => write_json_string(out, p),
            Schema::Struct(fields) => {
                out.push_str("{\"struct\":{");
                for (i, (name, schema)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(out, name);
                    out.push(':');
                    schema.write_json(out);
                }
                out.push_str("}}");
            }
            Schema::Enum(variants) => {
                out.push_str("{\"enum\":[");
                for (i, (name, schema)) in variants.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str("{\"struct\":{");
                    write_json_string(out, name);
                    out.push(':');
                    schema.write_json(out);
                    out.push_str("}}");
                }
                out.push_str("]}");
            }
            Schema::Option(inner) => {
                out.push_str("{\"option\":");
                inner.write_json(out);
                out.push('}');
            }
            Schema::Vec(inner) => {
                out.push_str("{\"array\":{\"type\":");
                inner.write_json(out);
                out.push_str("}}");
            }
            Schema::Array(inner, len) => {
                out.push_str("{\"array\":{\"type\":");
                inner.write_json(out);
                out.push_str(",\"len\":");
                out.push_str(&len.to_string());
                out.push_str("}}");
            }
            Schema::Ref(name) => {
                out.push_str("{\"$ref\":");
                write_json_string(out, name);
                out.push('}');
            }
        }
    }

    pub fn to_json(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }
}

/// Named type definitions collected while building an ABI document.
/// BTreeMap keeps the `types` section deterministic across builds.
#[derive(Debug, Default)]
pub struct TypeRegistry {
    types: BTreeMap<String, Schema>,
}

impl TypeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.types.contains_key(name)
    }

    pub fn insert(&mut self, name: &str, schema: Schema) {
        self.types.insert(name.to_string(), schema);
    }

    pub fn get(&self, name: &str) -> Option<&Schema> {
        self.types.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Schema)> {
        self.types.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    pub fn write_json(&self, out: &mut String) {
        out.push('{');
        for (i, (name, schema)) in self.types.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_json_string(out, name);
            out.push(':');
            schema.write_json(out);
        }
        out.push('}');
    }
}

pub fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str("\\u");
                let code = c as u32;
                for shift in [12u32, 8, 4, 0] {
                    let digit = (code >> shift) & 0xf;
                    out.push(char::from_digit(digit, 16).unwrap());
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
