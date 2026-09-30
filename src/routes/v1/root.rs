use crate::server::AppState;
use axum::{extract::State, http::StatusCode, Json};
use tracing::debug;

#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct RootResponse {
    version: String,
    schema_version: String,
}

/// Version information
///
/// Get information about the version of the server that is running.
#[utoipa::path(
    get,
    path = "/v1/",
    responses(
        (status = 200, description = "API version and schema version", body = RootResponse),
    ),
    tag = "meta"
)]
#[axum::debug_handler]
pub async fn root(State(state): State<AppState>) -> (StatusCode, Json<RootResponse>) {
    let schema_version = state
        .db
        .read(|conn| {
            conn.query_row(
                "SELECT name FROM migrations ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
        })
        .await;
    let schema_version = match schema_version {
        Ok(Ok(name)) => name,
        Ok(Err(e)) => {
            debug!("could not read the schema version: {e:?}");
            "unknown".to_string()
        }
        Err(e) => {
            debug!("could not read the schema version: {e:?}");
            "unknown".to_string()
        }
    };

    (
        StatusCode::OK,
        Json(RootResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
            schema_version,
        }),
    )
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod test {
    use crate::{db::migrations, test::TestBuilder};
    use anyhow::Result;
    use axum::http::StatusCode;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_get_root() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        let resp = client.get("http://localhost/v1/").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);

        let json = resp.json::<HashMap<String, String>>().await?;
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));

        // The schema version should be the last migration name
        let last_migration = migrations::MIGRATIONS
            .last()
            .map(|m| m.name)
            .unwrap_or("unknown");
        assert_eq!(json["schema_version"], last_migration);

        Ok(())
    }
}
