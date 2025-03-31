use crate::{db::SCHEMA_VERSION, server::AppState};
use axum::{extract::State, http::StatusCode, Json};
use clap::crate_version;

#[derive(serde::Serialize)]
pub struct RootResponse<'a> {
    version: &'a str,
    schema_version: &'a str,
}

const ROOT: RootResponse = RootResponse {
    version: crate_version!(),
    schema_version: SCHEMA_VERSION
};

/// Route handler for the root url, `/`.
#[axum::debug_handler]
pub async fn root(State(_state): State<AppState>) -> (StatusCode, Json<RootResponse<'static>>) {
    (StatusCode::OK, Json(ROOT))
}

#[cfg(test)]
mod test {
    use crate::test::TestBuilder;
    use anyhow::Result;

    #[tokio::test]
    async fn test_get_root() -> Result<()> {
        let _tc = TestBuilder::all().build()?;

        Ok(())
    }
}
