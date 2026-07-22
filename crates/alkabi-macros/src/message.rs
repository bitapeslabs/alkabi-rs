use proc_macro2::{Ident, Span, TokenStream};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{parenthesized, Data, DeriveInput, Error, Fields, LitInt, Token, Type};

use crate::util::{to_lower_camel_case, to_snake_case};

enum ReturnsSpec {
    /// `#[returns(borsh(T))]` — response data is borsh-serialized `T`.
    Borsh(Type),
    /// `#[returns(T)]` / `#[returns(T, U)]` — raw response bytes.
    Raw(Vec<Type>),
}

impl Parse for ReturnsSpec {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.peek(syn::Ident) && input.peek2(syn::token::Paren) {
            let fork = input.fork();
            let ident: Ident = fork.parse()?;
            if ident == "borsh" {
                let _: Ident = input.parse()?;
                let content;
                parenthesized!(content in input);
                let ty: Type = content.parse()?;
                return Ok(ReturnsSpec::Borsh(ty));
            }
        }
        let types = Punctuated::<Type, Token![,]>::parse_terminated(input)?;
        if types.is_empty() {
            return Err(input.error("#[returns(...)] requires at least one type"));
        }
        Ok(ReturnsSpec::Raw(types.into_iter().collect()))
    }
}

enum VariantFields {
    Unit,
    /// Named fields; `has_braces` distinguishes `Foo {}` from bare `Foo` for
    /// pattern construction.
    Named(Vec<(Ident, Type)>),
    /// Single unnamed field (only valid with `#[borsh]`).
    UnnamedSingle(Type),
}

struct VariantInfo {
    ident: Ident,
    opcode: u128,
    is_view: bool,
    is_borsh: bool,
    witness: Option<Type>,
    returns: Option<ReturnsSpec>,
    fields: VariantFields,
}

pub fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    let enum_ident = &input.ident;

    let Data::Enum(data) = &input.data else {
        return Err(Error::new_spanned(
            &input.ident,
            "AlkabiMessage can only be derived for enums",
        ));
    };

    let contract_ident = parse_contract_ident(&input)?;
    let contract_str = contract_ident.to_string();

    let mut variants = Vec::new();
    for variant in &data.variants {
        variants.push(parse_variant(variant)?);
    }

    let mut seen_opcodes = std::collections::HashSet::new();
    for v in &variants {
        if !seen_opcodes.insert(v.opcode) {
            return Err(Error::new_spanned(
                &v.ident,
                format!("Duplicate opcode {}", v.opcode),
            ));
        }
    }

    let from_opcode_arms: Vec<TokenStream> = variants.iter().map(from_opcode_arm).collect();
    let dispatch_arms: Vec<TokenStream> = variants.iter().map(dispatch_arm).collect();
    let collect_stmts: Vec<TokenStream> = variants.iter().flat_map(collect_stmts).collect();
    let method_exprs: Vec<TokenStream> = variants.iter().map(method_expr).collect();

    Ok(quote! {
        impl ::alkanes_runtime::message::MessageDispatch<#contract_ident> for #enum_ident {
            fn from_opcode(opcode: u128, __macro_inputs: Vec<u128>) -> ::anyhow::Result<Self> {
                match opcode {
                    #(#from_opcode_arms)*
                    _ => Err(::anyhow::anyhow!("Unknown opcode: {}", opcode)),
                }
            }

            fn dispatch(
                &self,
                responder: &#contract_ident,
            ) -> ::anyhow::Result<::alkanes_support::response::CallResponse> {
                match self {
                    #(#dispatch_arms)*
                }
            }

            fn export_abi() -> Vec<u8> {
                let mut __reg = ::alkabi::schema::TypeRegistry::new();
                #(#collect_stmts)*
                let __doc = ::alkabi::abi::AbiDocument {
                    contract: ::std::string::String::from(#contract_str),
                    types: __reg,
                    methods: ::std::vec![#(#method_exprs),*],
                };
                __doc.to_json().into_bytes()
            }
        }
    })
}

fn parse_contract_ident(input: &DeriveInput) -> syn::Result<Ident> {
    for attr in &input.attrs {
        if attr.path().is_ident("alkabi") {
            let mut contract = None;
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("contract") {
                    let value = meta.value()?;
                    contract = Some(value.parse::<Ident>()?);
                    Ok(())
                } else {
                    Err(meta.error("expected `contract = TypeName`"))
                }
            })?;
            if let Some(ident) = contract {
                return Ok(ident);
            }
        }
    }
    let name = input.ident.to_string();
    let stripped = name.strip_suffix("Message").unwrap_or(&name);
    if stripped.is_empty() {
        return Err(Error::new_spanned(
            &input.ident,
            "Cannot infer contract type; use #[alkabi(contract = TypeName)]",
        ));
    }
    Ok(Ident::new(stripped, input.ident.span()))
}

fn parse_variant(variant: &syn::Variant) -> syn::Result<VariantInfo> {
    let mut opcode = None;
    let mut is_view = false;
    let mut is_borsh = false;
    let mut witness = None;
    let mut returns = None;

    for attr in &variant.attrs {
        if attr.path().is_ident("opcode") {
            let lit: LitInt = attr.parse_args()?;
            opcode = Some(lit.base10_parse::<u128>()?);
        } else if attr.path().is_ident("view") {
            is_view = true;
        } else if attr.path().is_ident("borsh") {
            is_borsh = true;
        } else if attr.path().is_ident("witness") {
            witness = Some(attr.parse_args::<Type>()?);
        } else if attr.path().is_ident("returns") {
            returns = Some(attr.parse_args::<ReturnsSpec>()?);
        }
    }

    let opcode = opcode.ok_or_else(|| {
        Error::new_spanned(&variant.ident, "Missing #[opcode(n)] attribute")
    })?;

    let fields = match &variant.fields {
        Fields::Unit => VariantFields::Unit,
        Fields::Named(named) => VariantFields::Named(
            named
                .named
                .iter()
                .map(|f| (f.ident.clone().unwrap(), f.ty.clone()))
                .collect(),
        ),
        Fields::Unnamed(unnamed) => {
            if !is_borsh {
                return Err(Error::new_spanned(
                    &variant.ident,
                    "Tuple variants are only supported with #[borsh]",
                ));
            }
            if unnamed.unnamed.len() != 1 {
                return Err(Error::new_spanned(
                    &variant.ident,
                    "#[borsh] variants must have exactly one field",
                ));
            }
            VariantFields::UnnamedSingle(unnamed.unnamed[0].ty.clone())
        }
    };

    if is_borsh {
        match &fields {
            VariantFields::UnnamedSingle(_) => {}
            VariantFields::Named(named) if named.len() == 1 => {}
            _ => {
                return Err(Error::new_spanned(
                    &variant.ident,
                    "#[borsh] variants must have exactly one field (the params struct)",
                ));
            }
        }
    }

    if let Some(ReturnsSpec::Raw(types)) = &returns {
        if types.len() > 1 {
            for ty in types {
                if !is_fixed_width_int(ty) {
                    return Err(Error::new_spanned(
                        ty,
                        "Multi-value #[returns(...)] only supports fixed-width integers",
                    ));
                }
            }
        }
    }

    Ok(VariantInfo {
        ident: variant.ident.clone(),
        opcode,
        is_view,
        is_borsh,
        witness,
        returns,
        fields,
    })
}

fn is_fixed_width_int(ty: &Type) -> bool {
    if let Type::Path(path) = ty {
        if let Some(segment) = path.path.segments.last() {
            return matches!(
                segment.ident.to_string().as_str(),
                "u8" | "u16" | "u32" | "u64" | "u128" | "i8" | "i16" | "i32" | "i64" | "i128"
            );
        }
    }
    false
}

fn opcode_lit(opcode: u128) -> LitInt {
    LitInt::new(&format!("{}u128", opcode), Span::call_site())
}

fn from_opcode_arm(v: &VariantInfo) -> TokenStream {
    let ident = &v.ident;
    let opcode = opcode_lit(v.opcode);

    if v.is_borsh {
        return match &v.fields {
            VariantFields::UnnamedSingle(ty) => quote! {
                #opcode => {
                    let __params: #ty = ::alkabi::borsh_io::decode_words(&__macro_inputs)?;
                    Ok(Self::#ident(__params))
                }
            },
            VariantFields::Named(named) => {
                let (fname, ty) = &named[0];
                quote! {
                    #opcode => {
                        let __params: #ty = ::alkabi::borsh_io::decode_words(&__macro_inputs)?;
                        Ok(Self::#ident { #fname: __params })
                    }
                }
            }
            VariantFields::Unit => unreachable!(),
        };
    }

    match &v.fields {
        VariantFields::Unit => quote! {
            #opcode => Ok(Self::#ident),
        },
        VariantFields::Named(named) if named.is_empty() => quote! {
            #opcode => Ok(Self::#ident {}),
        },
        VariantFields::Named(named) => {
            let decodes = named.iter().map(|(fname, ty)| {
                quote! {
                    let #fname = <#ty as ::alkabi::legacy::LegacyDecode>::decode(&mut __reader)?;
                }
            });
            let fnames = named.iter().map(|(fname, _)| fname);
            quote! {
                #opcode => {
                    let mut __reader = ::alkabi::legacy::LegacyReader::new(&__macro_inputs);
                    #(#decodes)*
                    Ok(Self::#ident { #(#fnames),* })
                }
            }
        }
        VariantFields::UnnamedSingle(_) => unreachable!(),
    }
}

fn dispatch_arm(v: &VariantInfo) -> TokenStream {
    let ident = &v.ident;
    let method = format_ident!("{}", to_snake_case(ident));

    let (pattern, mut args): (TokenStream, Vec<TokenStream>) = match &v.fields {
        VariantFields::Unit => (quote! { Self::#ident }, vec![]),
        VariantFields::Named(named) if named.is_empty() => (quote! { Self::#ident {} }, vec![]),
        VariantFields::Named(named) if v.is_borsh => {
            let (fname, _) = &named[0];
            (quote! { Self::#ident { #fname } }, vec![quote! { #fname }])
        }
        VariantFields::Named(named) => {
            let fnames: Vec<&Ident> = named.iter().map(|(fname, _)| fname).collect();
            let args = fnames.iter().map(|fname| quote! { #fname.clone() }).collect();
            (quote! { Self::#ident { #(#fnames),* } }, args)
        }
        VariantFields::UnnamedSingle(_) => {
            (quote! { Self::#ident(__params) }, vec![quote! { __params }])
        }
    };

    let witness_prelude = v.witness.as_ref().map(|wty| {
        quote! {
            let __witness: #wty = ::alkabi::witness::decode_witness(responder)?;
        }
    });
    if v.witness.is_some() {
        args.push(quote! { &__witness });
    }

    let call = quote! { responder.#method(#(#args),*) };
    let finish = returns_finish(v);

    quote! {
        #pattern => {
            #witness_prelude
            let __result = #call?;
            #finish
        }
    }
}

/// The epilogue routing the handler's result through `finish_return`, which
/// both encodes it and type-checks it against the declaration. Void methods
/// are typed as `()` — handlers return `Result<()>` or
/// `Result<AlkabiResponse<()>>`; a raw `CallResponse` never passes.
fn returns_finish(v: &VariantInfo) -> TokenStream {
    match &v.returns {
        None => quote! {
            ::alkabi::abi_return::finish_return::<
                _,
                ::alkabi::abi_return::RawMode,
                (),
                _,
                _,
            >(__result, responder)
        },
        Some(ReturnsSpec::Borsh(ty)) => quote! {
            ::alkabi::abi_return::finish_return::<
                _,
                ::alkabi::abi_return::BorshMode,
                #ty,
                _,
                _,
            >(__result, responder)
        },
        Some(ReturnsSpec::Raw(types)) => {
            let rty = if types.len() == 1 {
                let ty = &types[0];
                quote! { #ty }
            } else {
                quote! { (#(#types),*) }
            };
            quote! {
                ::alkabi::abi_return::finish_return::<
                    _,
                    ::alkabi::abi_return::RawMode,
                    #rty,
                    _,
                    _,
                >(__result, responder)
            }
        }
    }
}

/// Statements registering every named type this variant references.
fn collect_stmts(v: &VariantInfo) -> Vec<TokenStream> {
    let mut stmts = Vec::new();

    match &v.fields {
        VariantFields::Named(named) => {
            for (_, ty) in named {
                stmts.push(quote! {
                    <#ty as ::alkabi::AlkabiType>::collect(&mut __reg);
                });
            }
        }
        VariantFields::UnnamedSingle(ty) => {
            stmts.push(quote! {
                <#ty as ::alkabi::AlkabiType>::collect(&mut __reg);
            });
        }
        VariantFields::Unit => {}
    }

    if let Some(ty) = &v.witness {
        stmts.push(quote! {
            <#ty as ::alkabi::AlkabiType>::collect(&mut __reg);
        });
    }

    match &v.returns {
        Some(ReturnsSpec::Borsh(ty)) => {
            stmts.push(quote! {
                <#ty as ::alkabi::AlkabiType>::collect(&mut __reg);
            });
        }
        Some(ReturnsSpec::Raw(types)) => {
            for ty in types {
                stmts.push(quote! {
                    <#ty as ::alkabi::AlkabiType>::collect(&mut __reg);
                });
            }
        }
        None => {}
    }

    stmts
}

fn method_expr(v: &VariantInfo) -> TokenStream {
    let name = to_lower_camel_case(&v.ident);
    let opcode = opcode_lit(v.opcode);
    let kind = if v.is_view {
        quote! { ::alkabi::abi::MethodKind::View }
    } else {
        quote! { ::alkabi::abi::MethodKind::Execute }
    };

    let input = match &v.fields {
        VariantFields::Unit => quote! { ::core::option::Option::None },
        VariantFields::Named(named) if named.is_empty() => quote! { ::core::option::Option::None },
        VariantFields::Named(named) if v.is_borsh => {
            let (_, ty) = &named[0];
            borsh_io_expr(ty)
        }
        VariantFields::UnnamedSingle(ty) => borsh_io_expr(ty),
        VariantFields::Named(named) => {
            let fields = named.iter().map(|(fname, ty)| {
                let fname_str = fname.to_string();
                quote! {
                    (#fname_str.to_string(), <#ty as ::alkabi::AlkabiType>::reference())
                }
            });
            quote! {
                ::core::option::Option::Some(::alkabi::abi::AbiIo {
                    mode: ::alkabi::abi::IoMode::Legacy,
                    schema: ::alkabi::schema::Schema::Struct(::std::vec![#(#fields),*]),
                })
            }
        }
    };

    let witness = match &v.witness {
        Some(ty) => borsh_io_expr(ty),
        None => quote! { ::core::option::Option::None },
    };

    let output = match &v.returns {
        None => quote! { ::core::option::Option::None },
        Some(ReturnsSpec::Borsh(ty)) => quote! {
            ::core::option::Option::Some(::alkabi::abi::AbiIo {
                mode: ::alkabi::abi::IoMode::Borsh,
                schema: <#ty as ::alkabi::AlkabiType>::reference(),
            })
        },
        Some(ReturnsSpec::Raw(types)) if types.len() == 1 => {
            let ty = &types[0];
            quote! {
                ::core::option::Option::Some(::alkabi::abi::AbiIo {
                    mode: ::alkabi::abi::IoMode::Raw,
                    schema: <#ty as ::alkabi::AlkabiType>::reference(),
                })
            }
        }
        Some(ReturnsSpec::Raw(types)) => {
            let fields = types.iter().enumerate().map(|(i, ty)| {
                let fname = format!("_{}", i);
                quote! {
                    (#fname.to_string(), <#ty as ::alkabi::AlkabiType>::reference())
                }
            });
            quote! {
                ::core::option::Option::Some(::alkabi::abi::AbiIo {
                    mode: ::alkabi::abi::IoMode::Raw,
                    schema: ::alkabi::schema::Schema::Struct(::std::vec![#(#fields),*]),
                })
            }
        }
    };

    quote! {
        ::alkabi::abi::AbiMethod {
            name: ::std::string::String::from(#name),
            opcode: #opcode,
            kind: #kind,
            input: #input,
            witness: #witness,
            output: #output,
        }
    }
}

fn borsh_io_expr(ty: &Type) -> TokenStream {
    quote! {
        ::core::option::Option::Some(::alkabi::abi::AbiIo {
            mode: ::alkabi::abi::IoMode::Borsh,
            schema: <#ty as ::alkabi::AlkabiType>::reference(),
        })
    }
}
