//! Proc-macro DSL for differential-testing accessors.
//!
//! Generates feature-gated `diff_*` accessor methods from `#[diff(...)]`
//! field tags on a `#[derive(DifferentialSubject)]` struct. See
//! `verification/db/flavors/turso/ACCESSORS.md` (aretta-books repo) for
//! the catalog this serves.
//!
//! # Supported shapes
//!
//! ## Phase 1 — scalar field-projection (Option<T>)
//!
//! ```ignore
//! #[derive(DifferentialSubject)]
//! struct Log {
//!     #[diff(private, durable, expose = [version: u8])]
//!     header: Option<Header>,
//! }
//! // Generates:
//! //   pub fn diff_header_version(&self) -> Option<u8> {
//! //       self.header.as_ref().map(|inner| inner.version)
//! //   }
//! ```
//!
//! Generated method name: `diff_<container_field>_<exposed_subfield>`.
//!
//! ## Phase 2 — SkipMap wrapper-projection
//!
//! ```ignore
//! #[derive(DifferentialSubject)]
//! struct Store {
//!     #[diff(private, snapshot = TxSnapshot)]
//!     txs: SkipMap<TxID, Tx>,
//!
//!     #[diff(private, snapshot = StateSnapshot, name = "finalized")]
//!     finalized_tx_states: SkipMap<TxID, TxState>,
//! }
//!
//! impl From<&Tx> for TxSnapshot { fn from(t: &Tx) -> Self { ... } }
//! impl From<&TxState> for StateSnapshot { fn from(s: &TxState) -> Self { ... } }
//!
//! // Generates:
//! //   pub fn diff_txs(&self) -> Vec<(TxID, TxSnapshot)> {
//! //       let mut out: Vec<(TxID, TxSnapshot)> = self.txs.iter()
//! //           .map(|e| (*e.key(), TxSnapshot::from(e.value())))
//! //           .collect();
//! //       out.sort_by_key(|(k, _)| *k);
//! //       out
//! //   }
//! //   pub fn diff_finalized(&self) -> Vec<(TxID, StateSnapshot)> { ... }
//! ```
//!
//! Default method name: `diff_<field>`. Override with `name =
//! "<suffix>"` (auto-prefixes `diff_`).
//!
//! Constraints:
//! - Field type must be `SkipMap<K, V>` (detected by trailing path segment).
//! - `K: Copy` (needed for `*entry.key()`).
//! - User must provide `impl From<&V> for SnapshotType`.
//! - Per-element sort of inner collections (e.g. a tx's write_set) is the
//!   user's responsibility inside their `From` impl.
//!
//! ## Deferred to Phase 3+
//!
//! - Parameterized lookup (`fn(K) -> Option<V>` with enum-variant match;
//!   catalog row 5).
//! - Lossy snapshots of non-`Clone` iterators (`MvccIterator` cursor state).
//! - Other collection types (`BTreeMap`, `HashMap`, `Vec`, ...).
//!
//! # Tag grammar
//!
//! ```text
//! #[diff(
//!     <flag>*,                                  // metadata: private | public | durable | scratch | lock | ...
//!     ( expose = [ <name>: <Type> ( , <name>: <Type> )* ] )?    // Phase 1
//!     ( snapshot = <SnapshotType> )?                            // Phase 2
//!     ( name = "<suffix>" )?                                    // method-name override
//! )]
//! ```
//!
//! `expose` and `snapshot` are mutually exclusive on the same field
//! (one targets an `Option<T>` field, the other a `SkipMap<K, V>` field).

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::{
    parse_macro_input, Data, DataStruct, DeriveInput, Field, Fields, GenericArgument, Ident,
    LitStr, Path, PathArguments, Type, TypePath,
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

        let field_ident = field
            .ident
            .as_ref()
            .expect("named field has an ident");

        // --- Phase 1: scalar field-projection from Option<T> ---
        if !spec.exposed.is_empty() {
            let inner = match option_inner_type(&field.ty) {
                Some(t) => t,
                None => {
                    return syn::Error::new_spanned(
                        &field.ty,
                        "#[diff(expose = [...])] only supports Option<T> fields in Phase 1; \
                         wrapper-projection uses `snapshot = ...` on a `SkipMap<K, V>` field",
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

        // --- Phase 2: SkipMap wrapper-projection ---
        if let Some(snapshot_ty) = &spec.snapshot {
            let (k_ty, _v_ty) = match skipmap_kv_types(&field.ty) {
                Some(kv) => kv,
                None => {
                    return syn::Error::new_spanned(
                        &field.ty,
                        "#[diff(snapshot = ...)] only supports SkipMap<K, V> fields in Phase 2; \
                         other collection types (BTreeMap, HashMap, ...) deferred",
                    )
                    .to_compile_error()
                    .into();
                }
            };

            let method_suffix = match &spec.name_override {
                Some(s) => s.clone(),
                None => field_ident.to_string(),
            };
            let method_name = Ident::new(
                &format!("diff_{}", method_suffix),
                spec.name_override
                    .as_ref()
                    .map(|_| Span::call_site())
                    .unwrap_or_else(|| field_ident.span()),
            );

            let doc = format!(
                "Differential-testing accessor — owned snapshot of `{0}` projected via \
                 `<{1} as From<&_>>::from`, sorted ascending by key.\n\
                 \n\
                 **NEVER use in production.** Takes locks and allocates per call. \
                 See `verification/db/flavors/turso/ACCESSORS.md` in the aretta-books \
                 repo for the catalog row.",
                field_ident,
                quote!(#snapshot_ty),
            );
            accessors.push(quote! {
                #[doc = #doc]
                pub fn #method_name(&self) -> ::std::vec::Vec<(#k_ty, #snapshot_ty)> {
                    let mut out: ::std::vec::Vec<(#k_ty, #snapshot_ty)> = self
                        .#field_ident
                        .iter()
                        .map(|entry| (*entry.key(), #snapshot_ty::from(entry.value())))
                        .collect();
                    out.sort_by_key(|(k, _)| *k);
                    out
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
    snapshot: Option<Path>,
    name_override: Option<String>,
}

struct ExposedSubfield {
    name: Ident,
    ty: Type,
}

fn parse_field_attrs(field: &Field) -> syn::Result<Option<FieldSpec>> {
    let mut exposed = Vec::new();
    let mut snapshot: Option<Path> = None;
    let mut name_override: Option<String> = None;
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
            } else if ident == "snapshot" {
                let value = meta.value()?; // consumes `=`
                let path: Path = value.parse()?;
                if snapshot.is_some() {
                    return Err(meta.error("#[diff(snapshot = ...)] specified more than once"));
                }
                snapshot = Some(path);
            } else if ident == "name" {
                let value = meta.value()?; // consumes `=`
                let lit: LitStr = value.parse()?;
                if name_override.is_some() {
                    return Err(meta.error("#[diff(name = ...)] specified more than once"));
                }
                name_override = Some(lit.value());
            } else {
                // Flag (private/public/durable/scratch/lock/...) — captured as
                // metadata only; codegen only fires on `expose` / `snapshot`.
                // If the flag carries a value (`durable_after = "..."`) we
                // tolerate but ignore it.
                if meta.input.peek(syn::Token![=]) {
                    let value = meta.value()?;
                    let _: proc_macro2::TokenStream = value.parse()?;
                }
            }
            Ok(())
        })?;
    }

    if !saw_diff {
        return Ok(None);
    }

    if !exposed.is_empty() && snapshot.is_some() {
        return Err(syn::Error::new_spanned(
            &field.ty,
            "#[diff(...)] cannot combine `expose = [...]` (Phase 1, scalar projection of \
             Option<T>) with `snapshot = ...` (Phase 2, wrapper projection of SkipMap<K, V>) \
             on the same field",
        ));
    }

    Ok(Some(FieldSpec {
        exposed,
        snapshot,
        name_override,
    }))
}

fn option_inner_type(ty: &Type) -> Option<&Type> {
    generic_inner_types(ty, "Option").and_then(|args| args.into_iter().next())
}

fn skipmap_kv_types(ty: &Type) -> Option<(&Type, &Type)> {
    let args = generic_inner_types(ty, "SkipMap")?;
    let mut it = args.into_iter();
    let k = it.next()?;
    let v = it.next()?;
    Some((k, v))
}

fn generic_inner_types<'a>(ty: &'a Type, container_ident: &str) -> Option<Vec<&'a Type>> {
    let Type::Path(TypePath { qself: None, path }) = ty else {
        return None;
    };
    let last = path.segments.last()?;
    if last.ident != container_ident {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    let inner: Vec<&Type> = args
        .args
        .iter()
        .filter_map(|a| match a {
            GenericArgument::Type(t) => Some(t),
            _ => None,
        })
        .collect();
    Some(inner)
}
