//! Procedural macros for the Cordis framework.
//!
//! - `#[service]` on a struct generates the `Service` impl (stable name)
//!   and a typed accessor extension trait (`ctx.database()`).
//! - `#[inject]` is a marker for function-level injection callbacks; the TS
//!   `@Inject` method decorator has no stable-Rust equivalent and maps to
//!   declaring the dependency in a plugin's `inject` list (see B6 notes).

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{DeriveInput, parse_macro_input};

fn to_snake_case(ident: &syn::Ident) -> String {
    let name = ident.to_string();
    let mut out = String::new();
    for (index, ch) in name.chars().enumerate() {
        if ch.is_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Generates the `Service` impl and a typed accessor for a struct.
#[proc_macro_attribute]
pub fn service(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    let name = &input.ident;
    let snake = to_snake_case(name);
    let ext_trait = format_ident!("{}ServiceExt", name);
    let method = format_ident!("{}", snake);
    let expanded = quote! {
        #input

        impl ::cordis_core::Service for #name {
            const NAME: &'static str = #snake;
        }

        /// Typed accessor for the [`#name`] service.
        pub trait #ext_trait {
            /// Returns the service registered on this context, if any.
            fn #method(&self) -> Option<std::rc::Rc<#name>>;
        }

        impl #ext_trait for ::cordis_core::Context {
            fn #method(&self) -> Option<std::rc::Rc<#name>> {
                self.get::<#name>()
            }
        }
    };
    TokenStream::from(expanded)
}

/// Marker for injection callbacks (see module docs).
#[proc_macro_attribute]
pub fn inject(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}
