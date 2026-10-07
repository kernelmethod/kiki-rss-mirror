//! Build script for kiki-rss.
//!
//! With the `default-plugins` feature, bundles the plugins named in
//! [`DEFAULT_PLUGINS`] into a zstd-compressed tarball, `$OUT_DIR/default-plugins.tar.zst`, which
//! `src/plugins/defaults.rs` embeds in the binary. Each plugin's directory
//! under `plugins/` is stored under its own name at the root of the archive.
//! `kiki init` then unpacks them into the new home directory's plugins
//! directory.
//!
//! The filter plugin's `plugin.wasm` isn't kept in the repository: this script
//! builds it from `plugins/filter-src` for `wasm32-unknown-unknown`, into
//! `$OUT_DIR/filter-plugin.wasm`, and bundles it as `filter/plugin.wasm`. Set
//! `KIKI_FILTER_PLUGIN_WASM` to the path of a prebuilt one to use that instead,
//! as the Nix build does.

fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(feature = "default-plugins")]
    default_plugins::build_default_plugins()?;
    Ok(())
}

#[cfg(feature = "default-plugins")]
mod default_plugins {
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

    /// How deep inside a plugin directory files are bundled. Matches the depth
    /// the server looks for source files at.
    const MAX_DEPTH: usize = 8;

    /// zstd compression level for the tarball: the highest, since it is
    /// small and compressed once per build.
    const ZSTD_LEVEL: i32 = 19;

    /// The target the filter plugin is built for. Kiki turns the core module
    /// it produces into a component when it loads it. Unlike `wasm32-wasip2`,
    /// nixpkgs' Rust toolchain ships its standard library.
    const WASM_TARGET: &str = "wasm32-unknown-unknown";

    /// Environment variable naming a prebuilt filter plugin to bundle instead
    /// of building one.
    const PREBUILT_FILTER: &str = "KIKI_FILTER_PLUGIN_WASM";

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

        let filter_wasm = filter_plugin(&root, &out_dir)?;

        let mut archive = tar::Builder::new(Vec::new());
        for name in DEFAULT_PLUGINS {
            let dir = root.join("plugins").join(name);
            let mut files = Vec::new();
            collect_files(&dir, &dir, 0, &mut files)?;
            if *name == "filter" {
                // Replaces any plugin.wasm left over from when it was committed.
                files.retain(|(relative, _)| relative != "plugin.wasm");
                files.push(("plugin.wasm".to_string(), filter_wasm.clone()));
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

    /// Builds the filter plugin from `plugins/filter-src` into
    /// `$OUT_DIR/filter-plugin.wasm`, or copies the prebuilt one
    /// [`PREBUILT_FILTER`] names there, and returns its path.
    ///
    /// The tests in `src/plugins/filter_tests.rs` and `src/plugins/defaults.rs`
    /// embed it from there.
    fn filter_plugin(root: &Path, out_dir: &Path) -> io::Result<PathBuf> {
        let dest = out_dir.join("filter-plugin.wasm");

        println!("cargo:rerun-if-env-changed={PREBUILT_FILTER}");
        if let Some(prebuilt) = std::env::var_os(PREBUILT_FILTER) {
            let prebuilt = PathBuf::from(prebuilt);
            println!("cargo:rerun-if-changed={}", prebuilt.display());
            fs::copy(&prebuilt, &dest).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("copying ${PREBUILT_FILTER} ({}): {e}", prebuilt.display()),
                )
            })?;
            return Ok(dest);
        }

        let src = root.join("plugins").join("filter-src");
        let sdk = root.join("sdk").join("rust").join("kiki-plugin");
        for path in [
            src.join("Cargo.toml"),
            src.join("Cargo.lock"),
            src.join("src"),
            sdk.join("Cargo.toml"),
            sdk.join("src"),
            root.join("wit"),
        ] {
            println!("cargo:rerun-if-changed={}", path.display());
        }

        let target_dir = out_dir.join("filter-plugin");
        let mut cargo = Command::new(env("CARGO"));
        cargo
            .arg("build")
            .arg("--release")
            .arg("--locked")
            .arg("--target")
            .arg(WASM_TARGET)
            .arg("--manifest-path")
            .arg(src.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(&target_dir);
        // Flags meant for Kiki's own build, such as cargo-llvm-cov's
        // instrumentation or a musl target, don't apply to the plugin.
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
                "building the filter plugin in plugins/filter-src failed ({status}). \
                 It needs the {WASM_TARGET} target (`rustup target add {WASM_TARGET}`); \
                 or set ${PREBUILT_FILTER} to a prebuilt plugin.wasm, or build without \
                 the default-plugins feature"
            )));
        }

        fs::copy(
            target_dir
                .join(WASM_TARGET)
                .join("release")
                .join("kiki_filter.wasm"),
            &dest,
        )?;
        Ok(dest)
    }

    /// Collects every regular file under `dir`, as its `/`-separated path
    /// relative to `base` together with its full path. Hidden files and editor
    /// backups (`*~`) are skipped.
    fn collect_files(
        base: &Path,
        dir: &Path,
        depth: usize,
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
            if file_name.starts_with('.') || file_name.ends_with('~') {
                continue;
            }

            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                collect_files(base, &path, depth + 1, files)?;
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
