//! The little of WASI that WebAssembly plugins get: randomness and clocks.
//!
//! Kiki gives plugins no files, network, environment or standard streams, but common
//! libraries reach for randomness and the time without being asked: Rust's
//! `std::collections::HashMap`, which the `regex` crate uses, seeds its hasher from
//! `wasi:random/insecure-seed`, and logging or timing code asks the clocks. These are
//! harmless, so Kiki defines them, for whichever WASI 0.2 release a plugin imports:
//!
//! * `wasi:random/random`, `wasi:random/insecure` and `wasi:random/insecure-seed`, from the
//!   kernel's random number generator;
//! * `wasi:clocks/wall-clock`'s `now` and `resolution`, and `wasi:clocks/monotonic-clock`'s
//!   `now` and `resolution`. Waiting on a clock is not supported.
//!
//! Everything else a plugin imports, Kiki defines as functions that trap if called, so
//! that a plugin built with a toolchain that imports more of WASI than it uses still
//! loads; see [`define`].

use super::State;
use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use wasmtime::component::types::ComponentItem;
use wasmtime::component::{Component, ComponentType, Linker, LinkerInstance, Lower, ResourceType};
use wasmtime::{Engine, StoreContextMut};

/// Most random bytes a plugin may ask for in one call.
const MAX_RANDOM_BYTES: u64 = 1024 * 1024;

/// `wasi:clocks/wall-clock`'s `datetime`.
#[derive(ComponentType, Lower, Clone, Copy)]
#[component(record)]
struct Datetime {
    seconds: u64,
    nanoseconds: u32,
}

fn random_bytes(len: u64) -> wasmtime::Result<Vec<u8>> {
    if len > MAX_RANDOM_BYTES {
        return Err(wasmtime::Error::msg(format!(
            "asked for {len} random bytes, more than {MAX_RANDOM_BYTES}"
        )));
    }
    let mut bytes = vec![0; len as usize];
    getrandom::fill(&mut bytes).map_err(|e| wasmtime::Error::msg(e.to_string()))?;
    Ok(bytes)
}

fn random_u64() -> wasmtime::Result<u64> {
    let mut bytes = [0; 8];
    getrandom::fill(&mut bytes).map_err(|e| wasmtime::Error::msg(e.to_string()))?;
    Ok(u64::from_le_bytes(bytes))
}

/// When the monotonic clock started: its `now` counts nanoseconds from here.
fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Define, in `linker`, every interface `component` imports besides Kiki's own, skipping
/// those named in `defined` and adding the rest: the parts of WASI described in the
/// [module documentation](self) as they are, and everything else as functions that trap
/// if called, and resources that cannot be made.
///
/// Each interface is defined whole, the first time a component imports it, since a
/// linker refuses to define one twice. Components importing the same interface, by
/// name and version, import the same functions.
pub(super) fn define(
    linker: &mut Linker<State>,
    engine: &Engine,
    component: &Component,
    defined: &mut HashSet<String>,
) -> wasmtime::Result<()> {
    let ty = component.component_type();
    for (name, item) in ty.imports(engine) {
        // Kiki's own interfaces are defined by the bindings.
        if name.starts_with("kiki:plugin/") || defined.contains(name) {
            continue;
        }
        match item {
            ComponentItem::ComponentInstance(interface) => {
                let mut instance = linker.instance(name)?;
                let provided = define_wasi(&mut instance, name)?;
                for (export, item) in interface.exports(engine) {
                    if !provided.contains(&export) {
                        stub(&mut instance, name, export, &item)?;
                    }
                }
            }
            item => stub(&mut linker.root(), "", name, &item)?,
        }
        defined.insert(name.to_string());
    }
    Ok(())
}

/// Define `item`, named `export` in the interface `interface` (empty for the component's
/// own imports), as a function that traps if called, or a resource that cannot be made.
fn stub(
    instance: &mut LinkerInstance<'_, State>,
    interface: &str,
    export: &str,
    item: &ComponentItem,
) -> wasmtime::Result<()> {
    match item {
        ComponentItem::ComponentFunc(_) => {
            let name = if interface.is_empty() {
                export.to_string()
            } else {
                format!("{interface}#{export}")
            };
            instance.func_new(export, move |_, _, _, _| {
                Err(wasmtime::Error::msg(format!(
                    "{name} is not available to Kiki plugins"
                )))
            })
        }
        ComponentItem::Resource(_) => {
            instance.resource(export, ResourceType::host::<()>(), |_, _| Ok(()))
        }
        // Types other than resources, and anything a plugin world can't import, need no
        // definition, or fail to instantiate with a clear error.
        _ => Ok(()),
    }
}

/// Define in `instance`, the interface named `name`, the WASI functions Kiki provides
/// for it, if any, returning their names.
fn define_wasi(
    instance: &mut LinkerInstance<'_, State>,
    name: &str,
) -> wasmtime::Result<&'static [&'static str]> {
    let Some((interface, version)) = name.split_once('@') else {
        return Ok(&[]);
    };
    if !version.starts_with("0.2.") {
        return Ok(&[]);
    }
    Ok(match interface {
        "wasi:random/random" => {
            instance.func_wrap(
                "get-random-bytes",
                |_: StoreContextMut<'_, State>, (len,): (u64,)| Ok((random_bytes(len)?,)),
            )?;
            instance.func_wrap("get-random-u64", |_: StoreContextMut<'_, State>, (): ()| {
                Ok((random_u64()?,))
            })?;
            &["get-random-bytes", "get-random-u64"]
        }
        "wasi:random/insecure" => {
            instance.func_wrap(
                "get-insecure-random-bytes",
                |_: StoreContextMut<'_, State>, (len,): (u64,)| Ok((random_bytes(len)?,)),
            )?;
            instance.func_wrap(
                "get-insecure-random-u64",
                |_: StoreContextMut<'_, State>, (): ()| Ok((random_u64()?,)),
            )?;
            &["get-insecure-random-bytes", "get-insecure-random-u64"]
        }
        "wasi:random/insecure-seed" => {
            instance.func_wrap("insecure-seed", |_: StoreContextMut<'_, State>, (): ()| {
                Ok(((random_u64()?, random_u64()?),))
            })?;
            &["insecure-seed"]
        }
        "wasi:clocks/wall-clock" => {
            instance.func_wrap("now", |_: StoreContextMut<'_, State>, (): ()| {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                Ok((Datetime {
                    seconds: now.as_secs(),
                    nanoseconds: now.subsec_nanos(),
                },))
            })?;
            instance.func_wrap("resolution", |_: StoreContextMut<'_, State>, (): ()| {
                Ok((Datetime {
                    seconds: 0,
                    nanoseconds: 1,
                },))
            })?;
            &["now", "resolution"]
        }
        "wasi:clocks/monotonic-clock" => {
            instance.func_wrap("now", |_: StoreContextMut<'_, State>, (): ()| {
                Ok((epoch().elapsed().as_nanos() as u64,))
            })?;
            instance.func_wrap("resolution", |_: StoreContextMut<'_, State>, (): ()| {
                Ok((1u64,))
            })?;
            &["now", "resolution"]
        }
        _ => &[],
    })
}
