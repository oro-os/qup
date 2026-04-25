//! Proc macros for `qup-embassy`.

#![forbid(unsafe_code)]

use std::collections::HashSet;

use heck::ToSnakeCase;
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Data, DataEnum, DeriveInput, Error, Fields, LitStr, Result, Variant, parse_macro_input};

/// Derives `qup_embassy::QupValue` for unit enums transported as strings.
#[proc_macro_derive(Value, attributes(qup))]
pub fn derive_value(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    match expand_value_derive(&input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn expand_value_derive(input: &DeriveInput) -> Result<TokenStream2> {
    let enum_data = match &input.data {
        Data::Enum(enum_data) => enum_data,
        Data::Struct(_) | Data::Union(_) => {
            return Err(Error::new_spanned(
                &input.ident,
                "`Value` can only be derived for enums",
            ));
        }
    };

    build_enum_impl(input, enum_data)
}

fn build_enum_impl(input: &DeriveInput, enum_data: &DataEnum) -> Result<TokenStream2> {
    if enum_data.variants.is_empty() {
        return Err(Error::new_spanned(
            &input.ident,
            "`Value` requires at least one unit variant",
        ));
    }

    let mut seen_wire_names = HashSet::new();
    let variants = enum_data
        .variants
        .iter()
        .map(|variant| parse_variant(variant, &mut seen_wire_names))
        .collect::<Result<Vec<_>>>()?;

    let mut default_variant = &variants[0].ident;
    let mut found_explicit_default = false;
    for variant in &variants {
        if !variant.is_default {
            continue;
        }

        if found_explicit_default {
            return Err(Error::new_spanned(
                &variant.ident,
                "multiple `#[qup(default)]` variants are not allowed",
            ));
        }

        default_variant = &variant.ident;
        found_explicit_default = true;
    }

    let max_wire_len = variants
        .iter()
        .map(|variant| variant.wire_name.value().len())
        .max()
        .unwrap_or(0usize);

    let enum_ident = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let encode_arms = variants.iter().map(|variant| {
        let ident = &variant.ident;
        let wire_name = &variant.wire_name;
        quote! {
            Self::#ident => #wire_name,
        }
    });
    let decode_arms = variants.iter().map(|variant| {
        let ident = &variant.ident;
        let wire_name = &variant.wire_name;
        quote! {
            #wire_name => ::core::result::Result::Ok(Self::#ident),
        }
    });

    Ok(quote! {
        impl #impl_generics ::qup_embassy::QupValue for #enum_ident #ty_generics #where_clause {
            const DEFAULT: Self = Self::#default_variant;
            const MAX_WIRE_LEN: usize = 3 + #max_wire_len;
            const MAX_STR_LEN: usize = #max_wire_len;

            fn encode(
                &self,
                buffer: &mut [u8],
            ) -> ::core::result::Result<usize, ::qup_embassy::WireValueError> {
                let value = match self {
                    #( #encode_arms )*
                };

                ::qup_embassy::__private::encode_str_value(value, buffer)
            }

            fn decode(
                value: ::qup_embassy::WireValueRef<'_>,
            ) -> ::core::result::Result<Self, ::qup_embassy::WireValueError> {
                match value {
                    ::qup_embassy::WireValueRef::Str(value) => match value {
                        #( #decode_arms )*
                        _ => ::core::result::Result::Err(::qup_embassy::WireValueError::TypeMismatch),
                    },
                    ::qup_embassy::WireValueRef::Bool(_)
                    | ::qup_embassy::WireValueRef::I64(_)
                    | ::qup_embassy::WireValueRef::OversizedStr => {
                        ::core::result::Result::Err(::qup_embassy::WireValueError::TypeMismatch)
                    }
                }
            }
        }
    })
}

fn parse_variant(
    variant: &Variant,
    seen_wire_names: &mut HashSet<String>,
) -> Result<ParsedVariant> {
    if !matches!(variant.fields, Fields::Unit) {
        return Err(Error::new_spanned(
            variant,
            "`Value` only supports unit enum variants",
        ));
    }

    let attributes = parse_variant_attributes(variant)?;
    let wire_name = attributes.wire_name;
    let wire_name_value = wire_name.value();
    if !seen_wire_names.insert(wire_name_value.clone()) {
        return Err(Error::new_spanned(
            &wire_name,
            format!("duplicate QUP wire name `{wire_name_value}`"),
        ));
    }

    Ok(ParsedVariant {
        ident: variant.ident.clone(),
        wire_name,
        is_default: attributes.is_default,
    })
}

fn parse_variant_attributes(variant: &Variant) -> Result<ParsedAttributes> {
    let mut override_name = None;
    let mut is_default = false;
    for attr in &variant.attrs {
        if !attr.path().is_ident("qup") {
            continue;
        }

        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                let value = meta.value()?.parse::<LitStr>()?;
                if override_name.is_some() {
                    return Err(meta.error("duplicate `name` attribute"));
                }

                override_name = Some(value);
                return Ok(());
            }

            if meta.path.is_ident("default") {
                if meta.input.peek(syn::Token![=]) {
                    return Err(meta.error("`default` does not take a value"));
                }
                if is_default {
                    return Err(meta.error("duplicate `default` attribute"));
                }

                is_default = true;
                return Ok(());
            }

            Err(meta.error("unsupported qup attribute; expected `default` and/or `name = \"...\"`"))
        })?;
    }

    let wire_name = override_name.unwrap_or_else(|| {
        LitStr::new(
            &variant.ident.to_string().to_snake_case(),
            variant.ident.span(),
        )
    });
    validate_wire_name(&wire_name)?;
    Ok(ParsedAttributes {
        wire_name,
        is_default,
    })
}

fn validate_wire_name(wire_name: &LitStr) -> Result<()> {
    let value = wire_name.value();
    if value.is_empty() {
        return Err(Error::new_spanned(
            wire_name,
            "QUP wire name cannot be empty",
        ));
    }
    if value.as_bytes().contains(&0x00) {
        return Err(Error::new_spanned(
            wire_name,
            "QUP wire name cannot contain NUL bytes",
        ));
    }
    if value.len() > usize::from(u16::MAX) {
        return Err(Error::new_spanned(
            wire_name,
            "QUP wire name exceeds str16 capacity",
        ));
    }
    Ok(())
}

struct ParsedVariant {
    ident: syn::Ident,
    wire_name: LitStr,
    is_default: bool,
}

struct ParsedAttributes {
    wire_name: LitStr,
    is_default: bool,
}

#[cfg(test)]
mod tests {
    use syn::parse_quote;

    use super::expand_value_derive;

    #[test]
    fn rejects_structs() {
        let input = parse_quote! {
            struct Nope;
        };

        let error = expand_value_derive(&input).expect_err("struct derives should fail");
        assert!(error.to_string().contains("can only be derived for enums"));
    }

    #[test]
    fn rejects_non_unit_variants() {
        let input = parse_quote! {
            enum Nope {
                Value(u8),
            }
        };

        let error = expand_value_derive(&input).expect_err("non-unit variants should fail");
        assert!(
            error
                .to_string()
                .contains("only supports unit enum variants")
        );
    }

    #[test]
    fn rejects_duplicate_wire_names() {
        let input = parse_quote! {
            enum Nope {
                FirstValue,
                #[qup(name = "first_value")]
                AnotherValue,
            }
        };

        let error = expand_value_derive(&input).expect_err("duplicate wire names should fail");
        assert!(error.to_string().contains("duplicate QUP wire name"));
    }

    #[test]
    fn rejects_multiple_default_variants() {
        let input = parse_quote! {
            enum Nope {
                #[qup(default)]
                FirstValue,
                #[qup(default)]
                SecondValue,
            }
        };

        let error = expand_value_derive(&input).expect_err("multiple defaults should fail");
        assert!(
            error
                .to_string()
                .contains("multiple `#[qup(default)]` variants are not allowed")
        );
    }

    #[test]
    fn emits_snake_case_and_manual_override_names() {
        let input = parse_quote! {
            enum VbusMode {
                Off,
                #[qup(name = "partial")]
                EnablePartial,
            }
        };

        let expanded = expand_value_derive(&input)
            .expect("unit enums should derive")
            .to_string();

        assert!(expanded.contains("const DEFAULT : Self = Self :: Off"));
        assert!(expanded.contains("\"off\""));
        assert!(expanded.contains("\"partial\""));
    }

    #[test]
    fn emits_explicit_default_variant_with_combined_qup_attr() {
        let input = parse_quote! {
            enum VbusMode {
                Off,
                On,
                #[qup(default, name = "partial")]
                EnablePartial,
                Full,
            }
        };

        let expanded = expand_value_derive(&input)
            .expect("unit enums should derive")
            .to_string();

        assert!(expanded.contains("const DEFAULT : Self = Self :: EnablePartial"));
        assert!(expanded.contains("\"partial\""));
    }
}
