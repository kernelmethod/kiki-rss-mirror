//! The `#[plugin]` attribute of the [`kiki-plugin`](https://docs.rs/kiki-plugin) crate,
//! which re-exports it as `kiki_plugin::plugin`; see its documentation there.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote, quote_spanned};
use syn::ext::IdentExt;
use syn::parse::ParseStream;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{parse_macro_input, Attribute, Ident, ImplItem, ItemImpl, Token};

/// Something the server calls a plugin for: an event, or a timer or scan the plugin
/// started.
struct Event {
    /// The name `#[on(...)]` gives it, as the Lua API names it.
    name: &'static str,
    /// Its `event-kind` variant, for an event a plugin says it handles; `None` for one
    /// delivered whenever it happens.
    kind: Option<&'static str>,
    /// The `plugin` world's export the server calls.
    export: &'static str,
    /// The export's parameters, by name and type.
    params: &'static [(&'static str, &'static str)],
    /// What the export returns.
    returns: Option<&'static str>,
    /// What the export returns when the plugin has no handler for it, if anything.
    default: &'static str,
}

const EVENTS: &[Event] = &[
    Event {
        name: "entry.parsed",
        kind: Some("EntryParsed"),
        export: "on_entry_parsed",
        params: &[("entry", "::kiki_plugin::Entry")],
        returns: None,
        default: "",
    },
    Event {
        name: "entry.ingest",
        kind: Some("EntryIngest"),
        export: "on_entry_ingest",
        params: &[("entry", "::kiki_plugin::Entry")],
        returns: Some("::std::option::Option<::kiki_plugin::Entry>"),
        default: "Some(entry)",
    },
    Event {
        name: "fetch.success",
        kind: Some("FetchSuccess"),
        export: "on_fetch_success",
        params: &[("event", "::kiki_plugin::FetchSuccess")],
        returns: None,
        default: "",
    },
    Event {
        name: "fetch.error",
        kind: Some("FetchError"),
        export: "on_fetch_error",
        params: &[("event", "::kiki_plugin::FetchError")],
        returns: None,
        default: "",
    },
    Event {
        name: "feed.added",
        kind: Some("FeedAdded"),
        export: "on_feed_added",
        params: &[("feed", "::kiki_plugin::FeedEvent")],
        returns: None,
        default: "",
    },
    Event {
        name: "feed.removed",
        kind: Some("FeedRemoved"),
        export: "on_feed_removed",
        params: &[("feed", "::kiki_plugin::FeedEvent")],
        returns: None,
        default: "",
    },
    Event {
        name: "plugin.load",
        kind: Some("PluginLoad"),
        export: "on_plugin_load",
        params: &[],
        returns: None,
        default: "",
    },
    Event {
        name: "fetch.schedule",
        kind: Some("FetchSchedule"),
        export: "on_fetch_schedule",
        params: &[("schedule", "::kiki_plugin::FetchSchedule")],
        returns: Some("::std::option::Option<u64>"),
        default: "None",
    },
    Event {
        name: "timer",
        kind: None,
        export: "on_timer",
        params: &[("id", "u32")],
        returns: None,
        default: "",
    },
    Event {
        name: "scan.entry",
        kind: None,
        export: "on_scan_entry",
        params: &[("scan", "u64"), ("entry", "::kiki_plugin::Entry")],
        returns: Some("::std::option::Option<::kiki_plugin::Entry>"),
        default: "None",
    },
    Event {
        name: "scan.done",
        kind: None,
        export: "on_scan_done",
        params: &[("scan", "u64"), ("summary", "::kiki_plugin::ScanSummary")],
        returns: None,
        default: "",
    },
];

/// Export a plugin, with the handlers in the `impl` block it is put on.
///
/// Put it on an inherent `impl` block of the type implementing `kiki_plugin::Plugin`, and
/// mark each handler in it with `#[on(<event>)]`. The events a plugin handles are those
/// with a handler, so there is no list of them to keep in step; `Plugin::wants` can leave
/// some out for a given config. Methods without `#[on]` are left as they are, and a crate
/// has one `#[plugin]` block, since it exports one plugin.
///
/// A handler takes `&mut self` or `&self`, then the event's arguments:
///
/// | `#[on(...)]`     | arguments                         | returns         |
/// |------------------|-----------------------------------|-----------------|
/// | `entry.parsed`   | `entry: Entry`                    |                 |
/// | `entry.ingest`   | `entry: Entry`                    | `Option<Entry>` |
/// | `fetch.success`  | `event: FetchSuccess`             |                 |
/// | `fetch.error`    | `event: FetchError`               |                 |
/// | `feed.added`     | `feed: FeedEvent`                 |                 |
/// | `feed.removed`   | `feed: FeedEvent`                 |                 |
/// | `plugin.load`    |                                   |                 |
/// | `fetch.schedule` | `schedule: FetchSchedule`         | `Option<u64>`   |
/// | `timer`          | `id: u32`                         |                 |
/// | `scan.entry`     | `scan: u64, entry: Entry`         | `Option<Entry>` |
/// | `scan.done`      | `scan: u64, summary: ScanSummary` |                 |
///
/// The first eight are the events of the Lua API, by the same names. `entry.ingest`
/// returns the entry, possibly changed, to keep it, or `None` to drop it;
/// `fetch.schedule` returns a longer wait before the feed's next fetch, in seconds, or
/// `None` to leave it. `timer` is called when a timer started with `host::every` is due,
/// and `scan.entry` and `scan.done` for a scan started with `host::start_scan`: only
/// system tags added to a scanned entry's `tags` are kept, and `None` leaves it alone.
///
/// ```ignore
/// use kiki_plugin::{plugin, Entry, FeedEvent, Plugin};
///
/// #[plugin]
/// impl Counter {
///     #[on(entry.ingest)]
///     fn count(&mut self, entry: Entry) -> Option<Entry> {
///         self.seen += 1;
///         Some(entry)
///     }
///
///     #[on(feed.removed)]
///     fn reset(&mut self, _feed: FeedEvent) {
///         self.seen = 0;
///     }
/// }
/// ```
///
/// An unknown event, two handlers for one event, or a handler whose signature doesn't
/// fit its event fails to compile.
#[proc_macro_attribute]
pub fn plugin(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut block = parse_macro_input!(item as ItemImpl);
    if !attr.is_empty() {
        let attr = TokenStream2::from(attr);
        return syn::Error::new(attr.span(), "#[plugin] takes no arguments")
            .to_compile_error()
            .into();
    }
    match expand(&mut block) {
        Ok(exports) => quote!(#block #exports).into(),
        Err(e) => {
            let e = e.to_compile_error();
            quote!(#block #e).into()
        }
    }
}

/// Take the `#[on(...)]` attributes out of `block`, and return the plugin's exports.
fn expand(block: &mut ItemImpl) -> syn::Result<TokenStream2> {
    if let Some((_, path, _)) = &block.trait_ {
        return Err(syn::Error::new(
            path.span(),
            "#[plugin] goes on an inherent impl block, holding the plugin's handlers, \
             not on an impl of a trait",
        ));
    }
    if !block.generics.params.is_empty() {
        return Err(syn::Error::new(
            block.generics.span(),
            "a plugin type cannot be generic",
        ));
    }

    // The handler of each event, by its index in EVENTS.
    let mut handlers: Vec<Option<(Ident, Span)>> = EVENTS.iter().map(|_| None).collect();
    let mut errors: Option<syn::Error> = None;
    let mut push = |e: syn::Error| match &mut errors {
        Some(errors) => errors.combine(e),
        None => errors = Some(e),
    };

    for item in &mut block.items {
        let ImplItem::Fn(method) = item else {
            continue;
        };
        let (on, rest): (Vec<Attribute>, Vec<Attribute>) = method
            .attrs
            .drain(..)
            .partition(|attr| attr.path().is_ident("on"));
        method.attrs = rest;
        for attr in on {
            let event = match parse_event(&attr) {
                Ok(event) => event,
                Err(e) => {
                    push(e);
                    continue;
                }
            };
            if method.sig.receiver().is_none() {
                push(syn::Error::new(
                    method.sig.span(),
                    "a handler takes `&mut self` or `&self`",
                ));
            }
            match &handlers[event] {
                Some((first, _)) => push(syn::Error::new(
                    attr.span(),
                    format!("`{}` already has a handler, `{first}`", EVENTS[event].name),
                )),
                None => handlers[event] = Some((method.sig.ident.clone(), method.sig.span())),
            }
        }
    }
    if let Some(errors) = errors {
        return Err(errors);
    }

    let ty = &block.self_ty;
    let kinds = EVENTS
        .iter()
        .zip(&handlers)
        .filter(|(_, handler)| handler.is_some())
        .filter_map(|(event, _)| event.kind)
        .map(|kind| {
            let kind = format_ident!("{kind}");
            quote!(::kiki_plugin::EventKind::#kind)
        });
    let exports = EVENTS.iter().zip(&handlers).map(|(event, handler)| {
        let export = format_ident!("{}", event.export);
        let names: Vec<Ident> = event
            .params
            .iter()
            .map(|(n, _)| format_ident!("{n}"))
            .collect();
        let types = event.params.iter().map(|(_, t)| path_of(t));
        let returns = event.returns.map(|r| {
            let r = path_of(r);
            quote!(-> #r)
        });
        let body = match handler {
            // Spanned to the handler, so that a signature that doesn't fit the event is
            // reported there.
            Some((method, span)) => {
                let args = names
                    .iter()
                    .map(|name| Ident::new(&name.to_string(), *span));
                quote_spanned! {*span=>
                    ::kiki_plugin::__with::<#ty, _>(|plugin| #ty::#method(plugin, #(#args),*))
                }
            }
            None => {
                let default: TokenStream2 = event.default.parse().unwrap();
                let unused = (!names.is_empty()).then(|| quote!(let _ = (#(&#names,)*);));
                quote!(#unused #default)
            }
        };
        quote! {
            fn #export(#(#names: #types),*) #returns {
                #body
            }
        }
    });

    Ok(quote! {
        const _: () = {
            struct KikiPluginExports;

            // The closure calling a handler without arguments is needed when it takes
            // `&self`, which the method itself, as a function, would not be accepted for.
            #[allow(clippy::redundant_closure)]
            impl ::kiki_plugin::bindings::Guest for KikiPluginExports {
                fn init(
                    config: ::std::string::String,
                ) -> ::std::result::Result<
                    ::std::vec::Vec<::kiki_plugin::EventKind>,
                    ::std::string::String,
                > {
                    ::kiki_plugin::__init::<#ty>(config, &[#(#kinds),*])
                }

                #(#exports)*
            }

            ::kiki_plugin::bindings::export!(
                KikiPluginExports with_types_in ::kiki_plugin::bindings
            );
        };
    })
}

/// The index in [`EVENTS`] of the event `#[on(...)]` names.
fn parse_event(attr: &Attribute) -> syn::Result<usize> {
    let parts = attr.parse_args_with(|input: ParseStream| {
        Punctuated::<Ident, Token![.]>::parse_separated_nonempty_with(input, Ident::parse_any)
    });
    let parts = parts.map_err(|e| {
        syn::Error::new(
            e.span(),
            format!(
                "expected an event, such as `#[on(entry.ingest)]`; {}",
                names()
            ),
        )
    })?;
    let name = parts
        .iter()
        .map(Ident::to_string)
        .collect::<Vec<_>>()
        .join(".");
    EVENTS
        .iter()
        .position(|event| event.name == name)
        .ok_or_else(|| {
            syn::Error::new(parts.span(), format!("unknown event `{name}`; {}", names()))
        })
}

/// The events `#[on(...)]` takes, for error messages.
fn names() -> String {
    let names: Vec<_> = EVENTS.iter().map(|e| format!("`{}`", e.name)).collect();
    format!("the events are {}", names.join(", "))
}

/// `ty`, a type in [`EVENTS`], as tokens.
fn path_of(ty: &str) -> TokenStream2 {
    ty.parse().unwrap()
}
