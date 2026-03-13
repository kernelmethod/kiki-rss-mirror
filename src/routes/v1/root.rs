use crate::server::AppState;
use axum::{extract::State, http::StatusCode, Json};
use clap::crate_version;

#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct RootResponse {
    version: String,
    schema_version: String,
}

/// Route handler for the root url, `/`.
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
    let schema_version = match state.conn_pool.get() {
        Ok(conn) => {
            let mut stmt =
                match conn.prepare("SELECT name FROM migrations ORDER BY id DESC LIMIT 1") {
                    Ok(s) => s,
                    Err(_) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(RootResponse {
                                version: crate_version!().to_string(),
                                schema_version: "unknown".to_string(),
                            }),
                        );
                    }
                };
            match stmt.query_row([], |row| row.get::<_, String>(0)) {
                Ok(name) => name,
                Err(_) => "unknown".to_string(),
            }
        }
        Err(_) => "unknown".to_string(),
    };

    (
        StatusCode::OK,
        Json(RootResponse {
            version: crate_version!().to_string(),
            schema_version,
        }),
    )
}

#[cfg(test)]
mod test {
    use crate::{db::migrations, test::TestBuilder};
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

        // The schema version should be the last migration name
        let last_migration = migrations::MIGRATIONS
            .last()
            .map(|m| m.name)
            .unwrap_or("unknown");
        assert_eq!(json["schema_version"], last_migration);

        Ok(())
    }
}
