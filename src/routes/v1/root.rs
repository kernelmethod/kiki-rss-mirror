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
    schema_version: SCHEMA_VERSION,
};

/// Route handler for the root url, `/`.
#[axum::debug_handler]
pub async fn root(State(_state): State<AppState>) -> (StatusCode, Json<RootResponse<'static>>) {
    (StatusCode::OK, Json(ROOT))
}

#[cfg(test)]
mod test {
    use crate::{db::SCHEMA_VERSION, test::TestBuilder};
    use anyhow::Result;
    use axum::http::StatusCode;
    use clap::crate_version;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_get_root() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        let resp = client.get("http://kiki/v1/").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);

        let json = resp.json::<HashMap<String, String>>().await?;
        assert_eq!(json["version"], crate_version!());
        assert_eq!(json["schema_version"], SCHEMA_VERSION);

        Ok(())
    }
}
