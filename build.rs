//! Build script for kiki-rss.
//!
//! With the `default-plugins` feature, bundles the plugins named in
//! [`DEFAULT_PLUGINS`] into a zstd-compressed tarball, `$OUT_DIR/default-plugins.tar.zst`, which
//! `src/plugins/defaults.rs` embeds in the binary. Each plugin's directory
//! under `plugins/` is stored under its own name at the root of the archive.
//! `kiki init` then unpacks them into the new home directory's plugins
//! directory.
//!
//! Plugins written in Rust don't keep their `plugin.wasm` in the repository.
//! A default plugin whose directory is also a crate, with a `Cargo.toml` and
//! `src/` beside its `manifest.toml`, and a member of the workspace in
//! `plugins/Cargo.toml`, is built for `wasm32-unknown-unknown` into
//! `$OUT_DIR/plugins-wasm/<name>.wasm`, and bundled as `<name>/plugin.wasm`
//! without its source. Set `KIKI_PLUGINS_WASM_DIR` to a directory
//! of prebuilt `<name>.wasm` files to bundle those instead, as the Nix build
//! does.

fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(feature = "default-plugins")]
    default_plugins::build_default_plugins()?;
    Ok(())
}

#[cfg(feature = "default-plugins")]
mod default_plugins {
    use std::collections::BTreeMap;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Plugins, by directory name under `plugins/`, that Kiki installs by
    /// default.
    const DEFAULT_PLUGINS: &[&str] = &[
        "adaptive-fetch",
        "auto-tag",
        "filter",
        "privacy",
        "retention",
        "sanitize",
    ];

    /// What a plugin written in Rust has in its directory besides the plugin
    /// itself: its crate, and whatever building it by hand leaves there,
    /// including a `plugin.wasm` the bundled one replaces. Not bundled.
    const CRATE_FILES: &[&str] = &["Cargo.toml", "Cargo.lock", "src", "target", "plugin.wasm"];

    /// How deep inside a plugin directory files are bundled. Matches the depth
    /// the server looks for source files at.
    const MAX_DEPTH: usize = 8;

    /// zstd compression level for the tarball: the highest, since it is
    /// small and compressed once per build.
    const ZSTD_LEVEL: i32 = 19;

    /// The target plugins written in Rust are built for. Kiki turns the core
    /// modules it produces into components when it loads them. Unlike
    /// `wasm32-wasip2`, nixpkgs' Rust toolchain ships its standard library.
    const WASM_TARGET: &str = "wasm32-unknown-unknown";

    /// Environment variable naming a directory of prebuilt plugins, as
    /// `<name>.wasm`, to bundle instead of building them.
    const PREBUILT_DIR: &str = "KIKI_PLUGINS_WASM_DIR";

    fn env(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| panic!("${name} is not set"))
    }

    /// Writes every plugin in [`DEFAULT_PLUGINS`] to
    /// `$OUT_DIR/default-plugins.tar.zst`.
    ///
    /// The archive is reproducible: files are added in sorted order, with fixed
    /// ownership, permissions and modification times.
    pub fn build_default_plugins() -> io::Result<()> {
        let root = PathBuf::from(env("CARGO_MANIFEST_DIR"));
        let out_dir = PathBuf::from(env("OUT_DIR"));

        let wasm = wasm_plugins(&root, &out_dir)?;

        let mut archive = tar::Builder::new(Vec::new());
        for name in DEFAULT_PLUGINS {
            let dir = root.join("plugins").join(name);
            let mut files = Vec::new();
            let skip: &[&str] = if wasm.contains_key(name) {
                CRATE_FILES
            } else {
                &[]
            };
            collect_files(&dir, &dir, 0, skip, &mut files)?;
            if let Some(path) = wasm.get(name) {
                files.push(("plugin.wasm".to_string(), path.clone()));
            }
            files.sort();

            for (relative, path) in files {
                let contents = fs::read(&path)?;
                let mut header = tar::Header::new_ustar();
                header.set_size(contents.len() as u64);
                header.set_mode(0o644);
                header.set_uid(0);
                header.set_gid(0);
                header.set_mtime(0);
                header.set_entry_type(tar::EntryType::Regular);
                archive.append_data(
                    &mut header,
                    format!("{name}/{relative}"),
                    contents.as_slice(),
                )?;
            }
        }

        let tarball = archive.into_inner()?;
        let compressed = zstd::encode_all(tarball.as_slice(), ZSTD_LEVEL)?;
        fs::write(out_dir.join("default-plugins.tar.zst"), compressed)
    }

    /// Builds every plugin in [`DEFAULT_PLUGINS`] whose directory is a crate
    /// into `$OUT_DIR/plugins-wasm/<name>.wasm`, or
    /// copies the prebuilt ones from [`PREBUILT_DIR`] there, and returns their
    /// paths by plugin name.
    ///
    /// Tests, such as those in `src/plugins/filter_tests.rs`, embed them from
    /// there.
    fn wasm_plugins(root: &Path, out_dir: &Path) -> io::Result<BTreeMap<&'static str, PathBuf>> {
        let plugins_dir = root.join("plugins");
        let names: Vec<&'static str> = DEFAULT_PLUGINS
            .iter()
            .copied()
            .filter(|name| plugins_dir.join(name).join("Cargo.toml").is_file())
            .collect();

        println!("cargo:rerun-if-env-changed={PREBUILT_DIR}");
        let prebuilt = std::env::var_os(PREBUILT_DIR).map(PathBuf::from);
        let release_dir = match &prebuilt {
            Some(_) => None,
            None if names.is_empty() => None,
            None => Some(build_workspace(root, &plugins_dir, &names, out_dir)?),
        };

        let wasm_dir = out_dir.join("plugins-wasm");
        fs::create_dir_all(&wasm_dir)?;
        let mut paths = BTreeMap::new();
        for name in names {
            let (from, hint) = match (&prebuilt, &release_dir) {
                (Some(dir), _) => {
                    let from = dir.join(format!("{name}.wasm"));
                    println!("cargo:rerun-if-changed={}", from.display());
                    (from, format!("is it missing from ${PREBUILT_DIR}?"))
                }
                (None, Some(dir)) => (
                    dir.join(format!("kiki_{}.wasm", name.replace('-', "_"))),
                    format!(
                        "is plugins/{name} in plugins/Cargo.toml's members, \
                         with its package named kiki-{name}?"
                    ),
                ),
                (None, None) => unreachable!("plugins are built when there are any"),
            };
            let dest = wasm_dir.join(format!("{name}.wasm"));
            fs::copy(&from, &dest).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!(
                        "copying the {name} plugin from {}: {e}; {hint}",
                        from.display()
                    ),
                )
            })?;
            paths.insert(name, dest);
        }
        Ok(paths)
    }

    /// Builds the workspace in `plugins/`, holding the crates of the plugins
    /// `names`, and returns the directory the `.wasm` files land in.
    fn build_workspace(
        root: &Path,
        plugins_dir: &Path,
        names: &[&str],
        out_dir: &Path,
    ) -> io::Result<PathBuf> {
        let sdk = root.join("sdk").join("rust").join("kiki-plugin");
        let mut inputs = vec![
            plugins_dir.join("Cargo.toml"),
            plugins_dir.join("Cargo.lock"),
            sdk.join("Cargo.toml"),
            sdk.join("src"),
            root.join("wit"),
        ];
        for name in names {
            let dir = plugins_dir.join(name);
            inputs.push(dir.join("Cargo.toml"));
            inputs.push(dir.join("src"));
        }
        for path in inputs {
            println!("cargo:rerun-if-changed={}", path.display());
        }

        let target_dir = out_dir.join("plugins-target");
        let mut cargo = Command::new(env("CARGO"));
        cargo
            .arg("build")
            .arg("--release")
            .arg("--locked")
            .arg("--workspace")
            .arg("--target")
            .arg(WASM_TARGET)
            .arg("--manifest-path")
            .arg(plugins_dir.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(&target_dir);
        // Flags meant for Kiki's own build, such as cargo-llvm-cov's
        // instrumentation or a musl target, don't apply to the plugins.
        for var in [
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_BUILD_RUSTFLAGS",
            "CARGO_BUILD_TARGET",
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_TARGET_DIR",
            "RUSTC_WORKSPACE_WRAPPER",
        ] {
            cargo.env_remove(var);
        }

        let status = cargo.status()?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "building the plugins in plugins/ failed ({status}). \
                 It needs the {WASM_TARGET} target (`rustup target add {WASM_TARGET}`); \
                 or set ${PREBUILT_DIR} to a directory of prebuilt <name>.wasm files, \
                 or build without the default-plugins feature"
            )));
        }
        Ok(target_dir.join(WASM_TARGET).join("release"))
    }

    /// Collects every regular file under `dir`, as its `/`-separated path
    /// relative to `base` together with its full path. Hidden files and editor
    /// backups (`*~`) are skipped, and so are the entries of `base` named in
    /// `skip`.
    fn collect_files(
        base: &Path,
        dir: &Path,
        depth: usize,
        skip: &[&str],
        files: &mut Vec<(String, PathBuf)>,
    ) -> io::Result<()> {
        if depth > MAX_DEPTH {
            return Ok(());
        }
        // Rerun when a file is added to or removed from the directory.
        println!("cargo:rerun-if-changed={}", dir.display());
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if file_name.starts_with('.')
                || file_name.ends_with('~')
                || (depth == 0 && skip.contains(&file_name.as_ref()))
            {
                continue;
            }

            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                collect_files(base, &path, depth + 1, skip, files)?;
            } else if file_type.is_file() {
                println!("cargo:rerun-if-changed={}", path.display());
                let relative = path
                    .strip_prefix(base)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                files.push((relative, path));
            }
        }
        Ok(())
    }
}
