use crate::server::AppState;
use axum::{extract::State, http::StatusCode, response::IntoResponse};

/// Shutdown server
///
/// Initiates a graceful shutdown of the server.
#[utoipa::path(
    post,
    path = "/v1/shutdown",
    responses(
        (status = 200, description = "Shutdown initiated"),
    ),
    tag = "meta"
)]
#[axum::debug_handler]
pub async fn shutdown(State(state): State<AppState>) -> impl IntoResponse {
    tracing::info!("Shutdown requested via API");
    state.cancel_token.cancel();
    (StatusCode::OK, "Shutdown initiated")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod test {
    use crate::test::TestBuilder;
    use anyhow::Result;
    use axum::http::StatusCode;

    /// POST /v1/shutdown should return 200 and stop the server.
    #[tokio::test]
    async fn test_shutdown() -> Result<()> {
        let mut tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client.post("http://localhost/v1/shutdown").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.text().await?, "Shutdown initiated");

        // The server thread should exit cleanly after shutdown.
        tc.server_handle
            .take()
            .unwrap()
            .join()
            .expect("panic in server thread")?;

        Ok(())
    }

    /// GET /v1/shutdown should return 405 Method Not Allowed.
    #[tokio::test]
    async fn test_shutdown_rejects_get() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client.get("http://localhost/v1/shutdown").send().await?;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

        Ok(())
    }
}
