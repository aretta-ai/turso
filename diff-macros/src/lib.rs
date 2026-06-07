//! Proc-macro DSL for differential-testing accessors.
//!
//! Generates feature-gated `diff_<container>_<exposed>` accessor methods from
//! `#[diff(...)]` field tags on a `#[derive(DifferentialSubject)]` struct.
//! See `verification/db/flavors/turso/ACCESSORS.md` (aretta-books repo) for
//! the catalog this serves.
//!
//! # Phase 1 scope (deliberate)
//!
//! Supports **scalar field-projection** only: a wrapper field of type
//! `Option<T>` with one or more exposed subfields. Example:
//!
//! ```ignore
//! use diff_macros::DifferentialSubject;
//!
//! struct Header { version: u8 }
//!
//! #[derive(DifferentialSubject)]
//! struct Log {
//!     #[diff(private, durable, expose = [version: u8])]
//!     header: Option<Header>,
//! }
//!
//! // Generated:
//! // impl Log {
//! //     pub fn diff_header_version(&self) -> Option<u8> {
//! //         self.header.as_ref().map(|inner| inner.version)
//! //     }
//! // }
//! ```
//!
//! # Deferred to Phase 2
//!
//! - Wrapper projection — `SkipMap<K, V>` → `Vec<(K, V)>` via owned snapshots
//!   (catalog rows 1-2). Requires a `Snapshot` trait or attribute-supplied
//!   wrapper type.
//! - Parameterized lookup — `fn(K) -> Option<V>` with enum-variant match
//!   (catalog row 5). Requires attribute-supplied predicate / projection
//!   closures.
//!
//! # Tag grammar
//!
//! ```text
//! #[diff( <flag>* , expose = [ <name>: <Type> ( , <name>: <Type> )* ] )]
//! ```
//!
//! Flags currently recognized as metadata (not consumed by codegen): `private`,
//! `public`, `durable`, `scratch`, `lock`. Unknown flags are accepted to
//! avoid blocking the catalog from describing fields the macro hasn't yet
//! learned to project; codegen only fires on `expose = [...]`.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::{
    parse_macro_input, Data, DataStruct, DeriveInput, Field, Fields, GenericArgument, Ident,
    PathArguments, Type, TypePath,
};

#[proc_macro_derive(DifferentialSubject, attributes(diff))]
pub fn derive_differential_subject(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let struct_name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let fields = match &input.data {
        Data::Struct(DataStruct {
            fields: Fields::Named(named),
            ..
        }) => &named.named,
        _ => {
            return syn::Error::new(
                Span::call_site(),
                "DifferentialSubject only supports structs with named fields",
            )
            .to_compile_error()
            .into();
        }
    };

    let mut accessors = Vec::new();
    for field in fields {
        let spec = match parse_field_attrs(field) {
            Ok(Some(s)) => s,
            Ok(None) => continue,
            Err(e) => return e.to_compile_error().into(),
        };

        if spec.exposed.is_empty() {
            continue;
        }

        let field_ident = field
            .ident
            .as_ref()
            .expect("named field has an ident");
        let inner = match option_inner_type(&field.ty) {
            Some(t) => t,
            None => {
                return syn::Error::new_spanned(
                    &field.ty,
                    "#[diff(expose = [...])] only supports Option<T> fields in Phase 1; \
                     wrapper-projection and parameterized lookup are deferred",
                )
                .to_compile_error()
                .into();
            }
        };

        for exposed in &spec.exposed {
            let method_name = Ident::new(
                &format!("diff_{}_{}", field_ident, exposed.name),
                exposed.name.span(),
            );
            let exposed_ident = &exposed.name;
            let exposed_type = &exposed.ty;
            let doc = format!(
                "Differential-testing accessor — projects `{0}.{1}` from `Option<{2}>`.\n\
                 \n\
                 **NEVER use in production.** Takes locks and allocates per call. \
                 See `verification/db/flavors/turso/ACCESSORS.md` in the aretta-books \
                 repo for the catalog row.",
                field_ident,
                exposed_ident,
                quote!(#inner),
            );
            accessors.push(quote! {
                #[doc = #doc]
                pub fn #method_name(&self) -> Option<#exposed_type> {
                    self.#field_ident.as_ref().map(|inner| inner.#exposed_ident)
                }
            });
        }
    }

    let out = quote! {
        impl #impl_generics #struct_name #ty_generics #where_clause {
            #(#accessors)*
        }
    };
    out.into()
}

struct FieldSpec {
    exposed: Vec<ExposedSubfield>,
}

struct ExposedSubfield {
    name: Ident,
    ty: Type,
}

fn parse_field_attrs(field: &Field) -> syn::Result<Option<FieldSpec>> {
    let mut exposed = Vec::new();
    let mut saw_diff = false;

    for attr in &field.attrs {
        if !attr.path().is_ident("diff") {
            continue;
        }
        saw_diff = true;
        attr.parse_nested_meta(|meta| {
            let ident = meta
                .path
                .get_ident()
                .cloned()
                .ok_or_else(|| meta.error("expected identifier inside #[diff(...)]"))?;
            if ident == "expose" {
                let value = meta.value()?; // consumes `=`
                let content;
                syn::bracketed!(content in value);
                while !content.is_empty() {
                    let name: Ident = content.parse()?;
                    content.parse::<syn::Token![:]>()?;
                    let ty: Type = content.parse()?;
                    exposed.push(ExposedSubfield { name, ty });
                    if !content.is_empty() {
                        content.parse::<syn::Token![,]>()?;
                    }
                }
            } else {
                // Flag (private/public/durable/scratch/lock/...) — captured as
                // metadata only; codegen only fires on `expose`. If the flag
                // carries a value (`durable_after = "..."`) we tolerate but
                // ignore it; future phases can route on these.
                if meta.input.peek(syn::Token![=]) {
                    let value = meta.value()?;
                    // Consume the rhs without committing to a type.
                    let _: proc_macro2::TokenStream = value.parse()?;
                }
            }
            Ok(())
        })?;
    }

    if !saw_diff {
        return Ok(None);
    }
    Ok(Some(FieldSpec { exposed }))
}

fn option_inner_type(ty: &Type) -> Option<&Type> {
    let Type::Path(TypePath { qself: None, path }) = ty else {
        return None;
    };
    let last = path.segments.last()?;
    if last.ident != "Option" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    let arg = args.args.first()?;
    let GenericArgument::Type(inner) = arg else {
        return None;
    };
    Some(inner)
}
