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
//! Everything else a plugin imports traps if called; see
//! [`wasmtime::component::Linker::define_unknown_imports_as_traps`].

use super::State;
use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use wasmtime::component::{Component, ComponentType, Linker, Lower};
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

/// Define, in `linker`, the parts of WASI described in the [module documentation](self)
/// that `component` imports, skipping those named in `defined` and adding the rest.
pub(super) fn define(
    linker: &mut Linker<State>,
    engine: &Engine,
    component: &Component,
    defined: &mut HashSet<String>,
) -> wasmtime::Result<()> {
    let ty = component.component_type();
    for (name, _) in ty.imports(engine) {
        let Some((interface, version)) = name.split_once('@') else {
            continue;
        };
        if !version.starts_with("0.2.") || defined.contains(name) {
            continue;
        }
        let mut instance = match interface {
            "wasi:random/random"
            | "wasi:random/insecure"
            | "wasi:random/insecure-seed"
            | "wasi:clocks/wall-clock"
            | "wasi:clocks/monotonic-clock" => linker.instance(name)?,
            _ => continue,
        };
        match interface {
            "wasi:random/random" => {
                instance.func_wrap(
                    "get-random-bytes",
                    |_: StoreContextMut<'_, State>, (len,): (u64,)| Ok((random_bytes(len)?,)),
                )?;
                instance.func_wrap("get-random-u64", |_: StoreContextMut<'_, State>, (): ()| {
                    Ok((random_u64()?,))
                })?;
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
            }
            "wasi:random/insecure-seed" => {
                instance.func_wrap("insecure-seed", |_: StoreContextMut<'_, State>, (): ()| {
                    Ok(((random_u64()?, random_u64()?),))
                })?;
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
            }
            "wasi:clocks/monotonic-clock" => {
                instance.func_wrap("now", |_: StoreContextMut<'_, State>, (): ()| {
                    Ok((epoch().elapsed().as_nanos() as u64,))
                })?;
                instance.func_wrap("resolution", |_: StoreContextMut<'_, State>, (): ()| {
                    Ok((1u64,))
                })?;
            }
            _ => {}
        }
        defined.insert(name.to_string());
    }
    Ok(())
}
