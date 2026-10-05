//! Build script for kiki-rss.
//!
//! With the `default-plugins` feature, bundles the plugins named in
//! [`DEFAULT_PLUGINS`] into a zstd-compressed tarball, `$OUT_DIR/default-plugins.tar.zst`, which
//! `src/plugins/defaults.rs` embeds in the binary. Each plugin's directory
//! under `plugins/` is stored under its own name at the root of the archive.
//! `kiki init` then unpacks them into the new home directory's plugins
//! directory.

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

    /// Plugins, by directory name under `plugins/`, that Kiki installs by
    /// default.
    const DEFAULT_PLUGINS: &[&str] = &[
        "adaptive-fetch",
        "auto-tag",
        "filter",
        "sanitize",
        "strip-tracking",
    ];

    /// How deep inside a plugin directory files are bundled. Matches the depth
    /// the server looks for source files at.
    const MAX_DEPTH: usize = 8;

    /// zstd compression level for the tarball: the highest, since it is
    /// small and compressed once per build.
    const ZSTD_LEVEL: i32 = 19;

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

        let mut archive = tar::Builder::new(Vec::new());
        for name in DEFAULT_PLUGINS {
            let dir = root.join("plugins").join(name);
            let mut files = Vec::new();
            collect_files(&dir, &dir, 0, &mut files)?;
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
