use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Error, Fields};

pub fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    let ident = &input.ident;
    let name_str = ident.to_string();

    let (schema_expr, collect_stmts) = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(named) => {
                let fields = named.named.iter().map(|f| {
                    let fname = f.ident.as_ref().unwrap().to_string();
                    let ty = &f.ty;
                    quote! {
                        (#fname.to_string(), <#ty as ::alkabi::AlkabiType>::reference())
                    }
                });
                let collects = named.named.iter().map(|f| {
                    let ty = &f.ty;
                    quote! { <#ty as ::alkabi::AlkabiType>::collect(reg); }
                });
                (
                    quote! {
                        ::alkabi::schema::Schema::Struct(::std::vec![#(#fields),*])
                    },
                    collects.collect::<Vec<_>>(),
                )
            }
            Fields::Unit => (
                quote! { ::alkabi::schema::Schema::Struct(::std::vec![]) },
                Vec::new(),
            ),
            Fields::Unnamed(_) => {
                return Err(Error::new_spanned(
                    ident,
                    "AlkabiType does not support tuple structs",
                ));
            }
        },
        Data::Enum(data) => {
            let mut variant_exprs = Vec::new();
            let mut collects = Vec::new();
            for variant in &data.variants {
                let vname = variant.ident.to_string();
                match &variant.fields {
                    Fields::Unit => {
                        variant_exprs.push(quote! {
                            (#vname.to_string(), ::alkabi::schema::Schema::Struct(::std::vec![]))
                        });
                    }
                    Fields::Named(named) => {
                        let fields = named.named.iter().map(|f| {
                            let fname = f.ident.as_ref().unwrap().to_string();
                            let ty = &f.ty;
                            quote! {
                                (#fname.to_string(), <#ty as ::alkabi::AlkabiType>::reference())
                            }
                        });
                        for f in &named.named {
                            let ty = &f.ty;
                            collects.push(quote! { <#ty as ::alkabi::AlkabiType>::collect(reg); });
                        }
                        variant_exprs.push(quote! {
                            (
                                #vname.to_string(),
                                ::alkabi::schema::Schema::Struct(::std::vec![#(#fields),*]),
                            )
                        });
                    }
                    Fields::Unnamed(unnamed) => {
                        if unnamed.unnamed.len() != 1 {
                            return Err(Error::new_spanned(
                                &variant.ident,
                                "AlkabiType enum tuple variants must have exactly one field",
                            ));
                        }
                        let ty = &unnamed.unnamed[0].ty;
                        collects.push(quote! { <#ty as ::alkabi::AlkabiType>::collect(reg); });
                        variant_exprs.push(quote! {
                            (#vname.to_string(), <#ty as ::alkabi::AlkabiType>::reference())
                        });
                    }
                }
            }
            (
                quote! {
                    ::alkabi::schema::Schema::Enum(::std::vec![#(#variant_exprs),*])
                },
                collects,
            )
        }
        Data::Union(_) => {
            return Err(Error::new_spanned(
                ident,
                "AlkabiType cannot be derived for unions",
            ));
        }
    };

    Ok(quote! {
        impl ::alkabi::AlkabiType for #ident {
            const NAME: ::core::option::Option<&'static str> =
                ::core::option::Option::Some(#name_str);

            fn schema() -> ::alkabi::schema::Schema {
                #schema_expr
            }

            fn collect(reg: &mut ::alkabi::schema::TypeRegistry) {
                if !reg.contains(#name_str) {
                    reg.insert(#name_str, <Self as ::alkabi::AlkabiType>::schema());
                    #(#collect_stmts)*
                }
            }
        }
    })
}
