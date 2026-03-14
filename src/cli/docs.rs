use anyhow::Result;
use clap::Args;
use std::io::Write;
use std::path::PathBuf;
use utoipa::OpenApi;

use crate::routes::v1::docs::ApiDoc;

/// Generate a static HTML page for the API documentation.
///
/// The generated page uses [Scalar](https://github.com/scalar/scalar) loaded
/// from a CDN to render the embedded OpenAPI specification. The resulting file
/// is fully self-contained and can be hosted on any static file server.
///
/// # Examples
///
/// Write to a file:
///
/// ```bash
/// kiki docs -o api-docs.html
/// ```
///
/// Write to stdout:
///
/// ```bash
/// kiki docs > api-docs.html
/// ```
#[derive(Args)]
pub struct DocsArgs {
    /// Path to write the HTML file to. If omitted, writes to stdout.
    #[arg(short, long)]
    output: Option<PathBuf>,
}

impl DocsArgs {
    /// Run the docs command.
    pub fn run(&self) -> Result<()> {
        let spec = ApiDoc::openapi().to_json()?;
        let html = generate_html(&spec);

        match &self.output {
            Some(path) => {
                let mut file = std::fs::File::create(path)?;
                file.write_all(html.as_bytes())?;
                eprintln!("Wrote API documentation to {}", path.display());
            }
            None => {
                let stdout = std::io::stdout();
                let mut handle = stdout.lock();
                handle.write_all(html.as_bytes())?;
            }
        }

        Ok(())
    }
}

/// Generate a self-contained HTML page embedding the OpenAPI spec with Scalar UI.
fn generate_html(openapi_spec: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>kiki-rss API Reference</title>
    <style>
      /* Customize the Scalar theme using CSS variables.
       * See https://github.com/scalar/scalar/blob/main/documentation/themes.md
       * for the full list of available variables. */
    </style>
  </head>
  <body>
    <script
      id="api-reference"
      type="application/json"
      data-configuration='{{"agent": {{"disabled": true}}, "mcp": {{"disabled": true}}, "hideClientButton": true}}'
    >
{openapi_spec}
    </script>
    <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
  </body>
</html>
"#,
        openapi_spec = openapi_spec
    )
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_generate_html_contains_spec() {
        let spec = r#"{"openapi":"3.0.0"}"#;
        let html = generate_html(spec);
        assert!(html.contains(spec));
        assert!(html.contains("@scalar/api-reference"));
        assert!(html.contains("kiki-rss API Reference"));
    }
}
