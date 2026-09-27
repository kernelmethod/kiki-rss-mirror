//! The `kiki opml` subcommands, for moving feed lists in and out of Kiki as
//! [OPML](https://en.wikipedia.org/wiki/OPML).
//!
//! Both subcommands work directly on the database, so they run whether or not
//! the server is up. A running server picks up imported feeds on its next
//! scheduling pass, since newly created feeds are immediately due for a fetch.
use crate::cli::paths::{self, Env};
use crate::db::ConnectionBuilder;
use crate::opml;
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Arguments for the `opml` subcommand.
#[derive(Args)]
pub struct OpmlArgs {
    #[command(subcommand)]
    command: OpmlCommand,
}

#[derive(Subcommand)]
enum OpmlCommand {
    /// Import feeds from an OPML file
    ///
    /// Folders in the OPML file become tags on the feeds inside them. Feeds
    /// whose URL is already in the database are skipped.
    Import(ImportArgs),

    /// Export all feeds as OPML
    ///
    /// Each tag becomes a folder containing the feeds with that tag; untagged
    /// feeds are written at the top level.
    Export(ExportArgs),
}

#[derive(Args)]
struct ImportArgs {
    /// OPML file to import, or `-` to read from standard input
    file: PathBuf,
}

#[derive(Args)]
struct ExportArgs {
    /// File to write the OPML to [default: standard output]
    #[arg(short, long)]
    output: Option<PathBuf>,
}

impl OpmlArgs {
    /// Run the `opml` subcommand.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened, the input cannot
    /// be read or parsed, or the output cannot be written.
    pub fn run(&self) -> Result<()> {
        // The same database `kiki serve` would open
        let database = paths::resolve_data_dir(&Env::from_process())?
            .path
            .join(paths::DB_FILE_NAME);

        match &self.command {
            OpmlCommand::Import(args) => {
                let summary = args.import(&database)?;
                println!(
                    "Imported {} feed(s); skipped {} already present.",
                    summary.imported.len(),
                    summary.skipped
                );
                Ok(())
            }
            OpmlCommand::Export(args) => args.export(&database),
        }
    }
}

impl ImportArgs {
    /// Read and parse the OPML input, then add its feeds to `database`.
    fn import(&self, database: &Path) -> Result<opml::ImportSummary> {
        let xml = read_input(&self.file)?;
        let feeds = opml::parse_opml(&xml)
            .with_context(|| format!("failed to parse OPML from {:?}", self.file))?;

        let mut conn = ConnectionBuilder::default()
            .at_path(database)
            .read_write()
            .build()
            .with_context(|| format!("failed to open database at {database:?}"))?;

        opml::import_feeds(&mut conn, &feeds)
            .with_context(|| format!("failed to import feeds into {database:?}"))
    }
}

impl ExportArgs {
    /// Write every feed in `database` as OPML.
    fn export(&self, database: &Path) -> Result<()> {
        let conn = ConnectionBuilder::default()
            .at_path(database)
            .build()
            .with_context(|| format!("failed to open database at {database:?}"))?;

        let feeds = opml::export_feeds(&conn)
            .with_context(|| format!("failed to read feeds from {database:?}"))?;
        let mut xml = opml::build_opml(&feeds)?;
        xml.push('\n');

        match &self.output {
            Some(path) => std::fs::write(path, xml)
                .with_context(|| format!("failed to write OPML to {path:?}"))?,
            None => std::io::stdout()
                .lock()
                .write_all(xml.as_bytes())
                .context("failed to write OPML to standard output")?,
        }

        Ok(())
    }
}

/// Read the whole of `path`, treating `-` as standard input.
fn read_input(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("failed to read OPML from standard input")?;
        Ok(buf)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("failed to read {path:?}"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempdir::TempDir;

    #[test]
    fn test_import_then_export() -> Result<()> {
        let dir = TempDir::new("kiki-opml")?;
        let database = dir.path().join("kiki.db");
        ConnectionBuilder::default()
            .at_path(&database)
            .create()
            .build()?;

        let input = dir.path().join("in.opml");
        std::fs::write(
            &input,
            r#"<opml version="2.0"><body>
  <outline text="tech">
    <outline text="Tech Blog" xmlUrl="https://example.com/tech.xml"/>
  </outline>
  <outline text="Plain" xmlUrl="https://example.com/plain.xml"/>
</body></opml>"#,
        )?;

        let import = ImportArgs { file: input };
        let summary = import.import(&database)?;
        assert_eq!(summary.imported.len(), 2);
        assert_eq!(summary.skipped, 0);

        // A second import finds every feed already present
        let summary = import.import(&database)?;
        assert!(summary.imported.is_empty());
        assert_eq!(summary.skipped, 2);

        let output = dir.path().join("out.opml");
        ExportArgs {
            output: Some(output.clone()),
        }
        .export(&database)?;

        let exported = opml::parse_opml(&std::fs::read_to_string(&output)?)?;
        assert_eq!(
            exported,
            vec![
                opml::OpmlFeed {
                    title: "Plain".into(),
                    url: "https://example.com/plain.xml".into(),
                    tags: vec![],
                },
                opml::OpmlFeed {
                    title: "Tech Blog".into(),
                    url: "https://example.com/tech.xml".into(),
                    tags: vec!["tech".into()],
                },
            ]
        );

        Ok(())
    }

    #[test]
    fn test_import_missing_database() {
        let dir = TempDir::new("kiki-opml").unwrap();
        let input = dir.path().join("in.opml");
        std::fs::write(&input, "<opml><body/></opml>").unwrap();

        let import = ImportArgs { file: input };
        assert!(import.import(&dir.path().join("missing.db")).is_err());
    }
}
