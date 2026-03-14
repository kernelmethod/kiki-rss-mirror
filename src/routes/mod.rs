pub mod v1;

use crate::server::AppState;
use axum::{http::StatusCode, Router};
use std::time::Duration;
use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};
use utoipa::OpenApi;

#[cfg(feature = "api-docs")]
const SCALAR_HTML: &str = r#"<!doctype html>
<html>
<head>
  <title>Scalar</title>
  <meta charset="utf-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1"/>
</head>
<body>
<script
  id="api-reference"
  type="application/json"
  data-configuration='{"agent": {"disabled": true}, "mcp": {"disabled": true}, "hideClientButton": true}'
>
  $spec
</script>
<script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
</body>
</html>
"#;

async fn api_fallback() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "Not Found")
}

pub fn create_router() -> Router<AppState> {
    let router = Router::new().nest("/v1/", v1::create_router());

    #[cfg(feature = "api-docs")]
    let router = {
        use utoipa_scalar::{Scalar, Servable};
        router
            .merge(Scalar::with_url("/docs", v1::docs::ApiDoc::openapi()).custom_html(SCALAR_HTML))
    };

    router.fallback(api_fallback).layer((
        TraceLayer::new_for_http(),
        TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, Duration::from_secs(10)),
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
