pub mod v1;

use crate::server::AppState;
use axum::{http::StatusCode, Router};
use std::time::Duration;
use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};
#[cfg(feature = "api-docs")]
use utoipa::OpenApi;

#[cfg(feature = "metrics")]
use axum::{middleware, routing::get};

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

/// The API's router, serving `state`.
///
/// Every route, the fallback included, is wrapped in
/// [`crate::auth::authorize`], which checks the token a request carries.
/// The router is returned finished, with the layer added last, so that
/// routes are only ever added here, inside it. A route added to the
/// returned router afterwards would not be wrapped: don't. The
/// `every_route_checks_tokens` test catches one that isn't.
pub fn create_router(state: AppState) -> Router {
    let router = Router::new().nest("/v1/", v1::create_router());

    #[cfg(feature = "api-docs")]
    let router = {
        use utoipa_scalar::{Scalar, Servable};
        router
            .merge(Scalar::with_url("/docs", v1::docs::ApiDoc::openapi()).custom_html(SCALAR_HTML))
    };

    #[cfg(feature = "metrics")]
    let router = router
        .route("/metrics", get(crate::metrics::handle_metrics))
        .layer(middleware::from_fn_with_state(
            state.metrics.clone(),
            crate::metrics::track_http,
        ));

    router
        .fallback(api_fallback)
        .layer((
            TraceLayer::new_for_http(),
            TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, Duration::from_secs(10)),
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::authorize,
        ))
        .with_state(state)
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

    /// Every route checks the token a request carries: a bad token is
    /// refused with `401` by every route in the OpenAPI document, and by
    /// `/docs`, `/metrics` and the fallback. A route the auth middleware
    /// does not wrap would answer some other way.
    #[tokio::test]
    async fn every_route_checks_tokens() -> Result<()> {
        use crate::routes::v1::docs::ApiDoc;
        use reqwest::Method;
        use utoipa::OpenApi;

        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // Path parameters are filled in with a value every route accepts
        // the shape of, so that requests reach the route rather than
        // failing to match it.
        let param = regex::Regex::new(r"\{[^}]+\}")?;
        let mut requests = vec![(Method::GET, "/v1/no-such-route".to_owned())];
        #[cfg(feature = "api-docs")]
        requests.push((Method::GET, "/docs".to_owned()));
        #[cfg(feature = "metrics")]
        requests.push((Method::GET, "/metrics".to_owned()));
        for (path, item) in &ApiDoc::openapi().paths.paths {
            for (method, op) in [
                (Method::GET, &item.get),
                (Method::PUT, &item.put),
                (Method::POST, &item.post),
                (Method::DELETE, &item.delete),
                (Method::PATCH, &item.patch),
            ] {
                if op.is_some() {
                    requests.push((method, param.replace_all(path, "1").into_owned()));
                }
            }
        }
        assert!(requests.len() > 50, "too few routes: {requests:?}");

        let mut unchecked = Vec::new();
        for (method, path) in &requests {
            let resp = client
                .request(method.clone(), format!("http://localhost{path}"))
                .bearer_auth("kiki_1_invalid")
                .send()
                .await?;
            if resp.status() != StatusCode::UNAUTHORIZED {
                unchecked.push(format!("{method} {path}: {}", resp.status()));
            }
        }
        assert!(
            unchecked.is_empty(),
            "routes that did not check the token: {unchecked:#?}"
        );
        Ok(())
    }

    #[cfg(feature = "metrics")]
    #[tokio::test]
    async fn test_metrics_endpoint_served() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // Touch a v1 route so the HTTP counters and duration histogram get a
        // sample — the exporter omits metric families that have never been
        // recorded, even if they've been described.
        let resp = client.get("http://localhost/v1/health").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = client.get("http://localhost/metrics").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            content_type.starts_with("text/plain"),
            "unexpected content-type: {content_type}"
        );

        let body = resp.text().await?;

        // Gauges are set during server startup, so they always appear.
        // HTTP counters/histograms appear once we've made a request above.
        for expected in [
            "kiki_http_requests_total",
            "kiki_http_request_duration_seconds",
            "kiki_task_queue_depth",
            "kiki_workers_total",
            "kiki_db_pool_connections",
            "kiki_build_info",
        ] {
            assert!(
                body.contains(expected),
                "missing metric family {expected} in:\n{body}"
            );
        }

        Ok(())
    }

    #[cfg(feature = "metrics")]
    #[tokio::test]
    async fn test_metrics_counts_http_requests() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // This server owns its own `Metrics` recorder, so the output starts
        // clean — we can assert that /v1/health's label is absent before we
        // hit the route and present after.
        let health_label = r#"kiki_http_requests_total{method="GET",route="/v1/health""#;

        let resp = client.get("http://localhost/metrics").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body_before = resp.text().await?;
        assert!(
            !body_before.contains(health_label),
            "/v1/health should not appear in metrics before it's fetched:\n{body_before}"
        );

        let resp = client.get("http://localhost/v1/health").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = client.get("http://localhost/metrics").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body_after = resp.text().await?;
        assert!(
            body_after.contains(health_label),
            "expected matched-path label for /v1/health after fetch:\n{body_after}"
        );
        // The scrape endpoint itself should be excluded from HTTP counters.
        assert!(
            !body_after.contains(r#"route="/metrics""#),
            "/metrics should not appear as an http_requests label"
        );

        Ok(())
    }

    #[cfg(feature = "metrics")]
    #[tokio::test]
    async fn test_metrics_profile_statements_and_processes() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;

        // The server queries its database as it starts, and samples its
        // processes' usage soon after.
        tc.wait_for_metric("kiki_db_statement_duration_seconds_count", |v| v > 0.0)
            .await?;
        #[cfg(target_os = "linux")]
        tc.wait_for_metric(
            r#"kiki_process_resident_memory_bytes{process="server"}"#,
            |v| v > 0.0,
        )
        .await?;
        #[cfg(target_os = "linux")]
        tc.wait_for_metric(
            r#"kiki_process_proportional_memory_bytes{process="server"}"#,
            |v| v > 0.0,
        )
        .await?;

        Ok(())
    }
}
