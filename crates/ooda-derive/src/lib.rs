//! `#[derive(Choice)]` — implements `ooda::ChoiceSpace` for a unit-variant
//! enum. See the `ooda` crate's `choice` module for the trait this backs,
//! and its docs for the derive's contract (unit variants only, a
//! description required per variant).
//!
//! No custom `#[ooda(...)]` helper attribute: a from-scratch inert attribute
//! registered via `attributes(...)` on this derive was found, empirically,
//! to make rustc 1.98.1 reject the attribute's own syntax with `expected
//! \",\"` on every use, reproducibly, even though the identical `key =
//! \"value\"` shape is accepted without issue under `#[serde(...)]` (a
//! helper attribute registered by `serde_derive`, applied via the same
//! `#[derive(...)]` list) on the same item. Root cause not isolated under
//! time pressure with a real downstream consumer (`legion-of-bom`) blocked
//! on this crate compiling at all; rather than ship broken syntax, this
//! derive now reads *only* `#[serde(rename = "...")]` /
//! `#[serde(rename_all = "...")]` (attributes already proven to work) for a
//! key override, and doc comments for the description. Revisit if a real
//! need for an `ooda`-specific override attribute comes up.

use heck::{
    ToKebabCase, ToLowerCamelCase, ToPascalCase, ToShoutyKebabCase, ToShoutySnakeCase, ToSnakeCase,
};
use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, Meta, parse_macro_input};

/// Derives `ooda::ChoiceSpace` for a unit-variant-only enum.
///
/// - **Key**: a variant's `#[serde(rename = "...")]`, else the enum's own
///   `#[serde(rename_all = "...")]` convention applied to the variant name,
///   else the bare variant identifier.
/// - **Description**: the variant's doc comment (`/// ...`, lines joined
///   with spaces) — required. A `Choice` option with no description would
///   silently degrade the endpoint's ability to pick correctly, so an
///   undocumented variant is a compile error, not a fallback to the bare
///   variant name.
#[proc_macro_derive(Choice)]
pub fn derive_choice(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(&input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            input,
            "ooda::Choice can only be derived for an enum",
        ));
    };

    let rename_all = enum_rename_all(input)?;

    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut variant_idents = Vec::new();

    for variant in &data.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(syn::Error::new_spanned(
                variant,
                "ooda::Choice only supports unit variants (no fields) -- a Choice option is a bounded name, not a payload",
            ));
        }

        let key = variant_key(variant, rename_all.as_deref())?;
        let description = variant_description(variant)?;

        pairs.push((key, description));
        variant_idents.push(variant.ident.clone());
    }

    let options_pairs = pairs.iter().map(|(k, d)| quote! { (#k, #d) });
    let from_key_arms = pairs
        .iter()
        .zip(variant_idents.iter())
        .map(|((k, _), ident)| quote! { #k => ::core::option::Option::Some(Self::#ident), });

    Ok(quote! {
        #[automatically_derived]
        impl ooda::ChoiceSpace for #name {
            fn options() -> ooda::Criteria {
                ooda::Criteria::from([#(#options_pairs),*])
            }

            fn from_key(key: &str) -> ::core::option::Option<Self> {
                match key {
                    #(#from_key_arms)*
                    _ => ::core::option::Option::None,
                }
            }
        }
    })
}

/// Reads `#[serde(rename_all = "...")]` off the enum itself, if present.
/// Any other content in a `#[serde(...)]` attribute (or one that doesn't
/// parse as a simple key/value list) is ignored rather than rejected: this
/// macro only needs the one field, and erroring on a foreign attribute it
/// doesn't otherwise understand would make an unrelated `serde` option a
/// compile break for `ooda::Choice`.
fn enum_rename_all(input: &DeriveInput) -> syn::Result<Option<String>> {
    for attr in &input.attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        let mut found = None;
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename_all") {
                let value = meta.value()?;
                let lit: syn::LitStr = value.parse()?;
                found = Some(lit.value());
            }
            Ok(())
        });
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

fn variant_key(variant: &syn::Variant, rename_all: Option<&str>) -> syn::Result<String> {
    if let Some(key) = serde_rename(variant)? {
        return Ok(key);
    }
    let raw = variant.ident.to_string();
    Ok(match rename_all {
        Some("lowercase") => raw.to_lowercase(),
        Some("UPPERCASE") => raw.to_uppercase(),
        Some("PascalCase") => raw.to_pascal_case(),
        Some("camelCase") => raw.to_lower_camel_case(),
        Some("snake_case") => raw.to_snake_case(),
        Some("SCREAMING_SNAKE_CASE") => raw.to_shouty_snake_case(),
        Some("kebab-case") => raw.to_kebab_case(),
        Some("SCREAMING-KEBAB-CASE") => raw.to_shouty_kebab_case(),
        _ => raw,
    })
}

fn serde_rename(variant: &syn::Variant) -> syn::Result<Option<String>> {
    for attr in &variant.attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        let mut found = None;
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                let value = meta.value()?;
                let lit: syn::LitStr = value.parse()?;
                found = Some(lit.value());
            }
            Ok(())
        });
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

fn variant_description(variant: &syn::Variant) -> syn::Result<String> {
    let doc = doc_comment(&variant.attrs);
    if !doc.is_empty() {
        return Ok(doc);
    }
    Err(syn::Error::new_spanned(
        variant,
        format!(
            "variant `{}` needs a doc comment (`/// ...`) -- a Choice option with no description degrades the endpoint's ability to pick it correctly",
            variant.ident
        ),
    ))
}

fn doc_comment(attrs: &[syn::Attribute]) -> String {
    let mut lines = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let Meta::NameValue(nv) = &attr.meta {
            if let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
            {
                let line = s.value();
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    lines.push(trimmed.to_owned());
                }
            }
        }
    }
    lines.join(" ")
}
