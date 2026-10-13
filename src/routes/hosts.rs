//! The `Host` check that guards a reverse proxy in front of the API
//! against DNS rebinding.
//!
//! The API listens only on a Unix socket, which no browser can reach, so
//! by default it answers whatever host a request names. A reverse proxy
//! that serves the API over TCP changes that: a site whose domain's DNS
//! record is switched to the proxy's address after the page loads can
//! then send requests to the API that the browser counts as same-origin,
//! and read the answers. Such a request still names the attacker's domain
//! in its `Host` header, though, so once the config lists the hosts the
//! proxy is reached at, the API answers only those. The list is
//! [`ApiSettings::allowed_hosts`](crate::config::ApiSettings::allowed_hosts),
//! or by default the shared `http.allowed_hosts`, which the web UI also
//! answers to, since the two usually share a name; see
//! [`Settings::api_allowed_hosts`](crate::config::Settings::api_allowed_hosts).

use crate::config::hosts;
use crate::server::AppState;
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Middleware that refuses, with `400 Bad Request`, any request that does
/// not name `localhost` or one of the API's allowed hosts in its `Host`
/// header, or for HTTP/2 its `:authority`, while there are any (see
/// [`Settings::api_allowed_hosts`](crate::config::Settings::api_allowed_hosts)).
/// A request that names no host is refused too. With none, every request
/// is let through.
pub async fn check_host(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let settings = state.config.current();
    let allowed = settings.api_allowed_hosts();
    if allowed.is_empty() {
        return next.run(req).await;
    }
    let authority = hosts::request_authority(&req);
    if authority.is_some_and(|a| hosts::allows(allowed, a)) {
        return next.run(req).await;
    }
    tracing::warn!(
        host = authority.unwrap_or("<none>"),
        "refused an API request for a host not in http.allowed_hosts (or api.allowed_hosts, if set)"
    );
    (
        StatusCode::BAD_REQUEST,
        "The API does not answer to this host name. Add it to `allowed_hosts` \
         under `[http]` in kiki.toml (or under `[api]`, if that is set) to \
         allow it.",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use crate::test::{TestBuilder, TestConfig};
    use anyhow::Result;
    use axum::http::{header, StatusCode};

    /// A server whose `kiki.toml` holds `config`.
    fn server_with_config(config: &str) -> Result<TestConfig> {
        let tc = TestBuilder::default().init_database().build()?;
        std::fs::write(
            tc.config_dir().join(crate::config::CONFIG_FILE_NAME),
            config,
        )?;
        tc.init_server()
    }

    /// The status of a `GET` for `path` naming `host` in its `Host` header.
    async fn status(client: &reqwest::Client, path: &str, host: &str) -> Result<StatusCode> {
        let resp = client
            .get(format!("http://localhost{path}"))
            .header(header::HOST, host)
            .send()
            .await?;
        Ok(resp.status())
    }

    #[tokio::test]
    async fn any_host_is_answered_without_allowed_hosts() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        for host in ["localhost", "kiki", "evil.example", "evil.example:8080"] {
            assert_eq!(
                status(&client, "/v1/health", host).await?,
                StatusCode::OK,
                "{host}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn only_allowed_hosts_are_answered() -> Result<()> {
        let tc = server_with_config(
            "[api]\nallowed_hosts = [\"kiki.example.com\", \"*.home.example\"]\n",
        )?;
        let client = tc.client()?;
        for host in [
            "kiki.example.com",
            "KIKI.example.com.:443",
            "rss.home.example",
            "localhost",
            "127.0.0.1:8080",
            "[::1]",
        ] {
            assert_eq!(
                status(&client, "/v1/feeds", host).await?,
                StatusCode::OK,
                "{host}"
            );
        }
        for host in [
            "evil.example",
            "kiki.example.com.evil.example",
            "home.example",
            "kiki",
            "kiki.example.com:",
            "",
        ] {
            for path in ["/v1/feeds", "/v1/health", "/no/such/route"] {
                assert_eq!(
                    status(&client, path, host).await?,
                    StatusCode::BAD_REQUEST,
                    "{host} {path}"
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn the_shared_hosts_are_used_unless_the_api_sets_its_own() -> Result<()> {
        let tc = server_with_config("[http]\nallowed_hosts = [\"kiki.lan\"]\n")?;
        let client = tc.client()?;
        assert_eq!(
            status(&client, "/v1/health", "kiki.lan").await?,
            StatusCode::OK
        );
        assert_eq!(
            status(&client, "/v1/health", "evil.example").await?,
            StatusCode::BAD_REQUEST
        );

        let tc = server_with_config(
            "[http]\nallowed_hosts = [\"kiki.lan\"]\n[api]\nallowed_hosts = []\n",
        )?;
        let client = tc.client()?;
        assert_eq!(
            status(&client, "/v1/health", "evil.example").await?,
            StatusCode::OK
        );
        Ok(())
    }
}
