mod v1;

use crate::server::AppState;
use axum::{http::StatusCode, Router};
use std::time::Duration;
use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};

async fn api_fallback() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "Not Found")
}

pub fn create_router() -> Router<AppState> {
    Router::new()
        .nest("/v1/", v1::create_router())
        .fallback(api_fallback)
        .layer((
            TraceLayer::new_for_http(),
            TimeoutLayer::new(Duration::from_secs(10)),
        ))
}

#[cfg(test)]
pub mod test {
    use crate::test::TestBuilder;
    use anyhow::Result;
    use axum::http::StatusCode;

    /// When we retrieve a random, non-existent URL, we should get
    /// a 404 Not Found response.
    #[tokio::test]
    async fn test_get_random_url() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        let resp = client
            .get("http:/iYZ89sNg_4HKcOHVebrbap775AjlQEkXDQQfTNAt2hM")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(resp.text().await?, "Not Found");

        Ok(())
    }
}
