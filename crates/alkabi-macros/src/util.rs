use proc_macro2::Ident;

/// `GetTotalSupply` -> `get_total_supply` (matches upstream alkanes-macros).
pub fn to_snake_case(ident: &Ident) -> String {
    let s = ident.to_string();
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `GetTotalSupply` -> `getTotalSupply` (the name the TS client exposes).
pub fn to_lower_camel_case(ident: &Ident) -> String {
    let s = ident.to_string();
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
        None => s,
    }
}
