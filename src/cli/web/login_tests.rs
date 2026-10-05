//! Tests of logging in to the web UI with an API token.

use super::hosts::AllowedHosts;
use super::login::Gate;
use super::server::{api_client, serve_ui_with};
use crate::test::{TestBuilder, TestConfig};
use anyhow::{Context, Result};
use axum::http::{header, StatusCode};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// A web UI that requires login, in front of a test server.
struct Ui {
    tc: TestConfig,
    addr: SocketAddr,
    cancel: CancellationToken,
    /// A browser that follows no redirects, so tests can see them.
    browser: reqwest::Client,
}

impl Drop for Ui {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Ui {
    async fn start(gate: fn(reqwest::Client) -> Gate) -> Result<Self> {
        let tc = TestBuilder::all().build()?;
        // Wait for the server to start.
        tc.client()?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let cancel = CancellationToken::new();
        tokio::spawn(serve_ui_with(
            listener,
            gate(api_client(&tc.socket_path())?),
            AllowedHosts::default(),
            cancel.clone(),
        ));
        let browser = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()?;
        Ok(Ui {
            tc,
            addr,
            cancel,
            browser,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    async fn get(&self, path: &str, cookie: Option<&str>) -> Result<reqwest::Response> {
        let mut req = self.browser.get(self.url(path));
        if let Some(cookie) = cookie {
            req = req.header(header::COOKIE, cookie);
        }
        Ok(req.send().await?)
    }

    /// Log in with `token`, and return the response.
    async fn log_in(&self, token: &str) -> Result<reqwest::Response> {
        Ok(self
            .browser
            .post(self.url("/login"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form_body(token))
            .send()
            .await?)
    }

    /// Log in with `token`, and return the session cookie to send.
    async fn session(&self, token: &str) -> Result<String> {
        let resp = self.log_in(token).await?;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .context("no session cookie")?;
        Ok(cookie.split(';').next().context("empty cookie")?.to_owned())
    }
}

/// The login form's body for `token`.
fn form_body(token: &str) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("token", token)
        .finish()
}

fn location(resp: &reqwest::Response) -> Option<&str> {
    resp.headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
}

#[tokio::test]
async fn pages_need_a_session() -> Result<()> {
    let ui = Ui::start(Gate::login_required).await?;

    let resp = ui.get("/", None).await?;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&resp), Some("/login"));
    let resp = ui.get("/", Some("kiki_session=forged")).await?;
    assert_eq!(location(&resp), Some("/login"));

    let resp = ui
        .browser
        .post(ui.url("/entries/read"))
        .header("Sec-Fetch-Site", "same-origin")
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = ui.get("/login", None).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    let page = resp.text().await?;
    assert!(page.contains("name=\"token\""), "{page}");
    // The login page is rendered for nobody, so it offers nothing else.
    assert!(page.contains("<body data-scopes=\"\">"), "{page}");
    Ok(())
}

#[tokio::test]
async fn bad_tokens_do_not_log_in() -> Result<()> {
    let ui = Ui::start(Gate::login_required).await?;
    for token in ["", "nonsense", "kiki_1_wrong"] {
        let resp = ui.log_in(token).await?;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{token:?}");
        assert!(resp.headers().get(header::SET_COOKIE).is_none());
        assert!(resp.text().await?.contains("not valid"));
    }
    Ok(())
}

#[tokio::test]
async fn other_sites_cannot_log_in_or_out() -> Result<()> {
    let ui = Ui::start(Gate::login_required).await?;
    let token = ui.tc.create_token("phone", "reader")?;
    for path in ["/login", "/logout"] {
        let resp = ui
            .browser
            .post(ui.url(path))
            .header("Sec-Fetch-Site", "cross-site")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form_body(&token))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{path}");
        assert!(resp.headers().get(header::SET_COOKIE).is_none(), "{path}");
    }
    Ok(())
}

#[tokio::test]
async fn a_session_acts_with_its_token() -> Result<()> {
    let ui = Ui::start(Gate::login_required).await?;
    let token = ui.tc.create_token("phone", "reader")?;

    let resp = ui.log_in(&token).await?;
    assert_eq!(location(&resp), Some("/"));
    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .context("no cookie")?
        .to_owned();
    for attribute in ["HttpOnly", "SameSite=Strict", "Path=/"] {
        assert!(set_cookie.contains(attribute), "{set_cookie}");
    }
    assert!(!set_cookie.contains("Secure"), "{set_cookie}");
    let cookie = ui.session(&token).await?;

    let resp = ui.get("/", Some(&cookie)).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let page = resp.text().await?;
    assert!(page.contains("data-scopes=\"read state\""), "{page}");
    assert!(page.contains(">phone</span>"), "{page}");
    assert!(page.contains("class=\"logout\""), "{page}");

    // Plugins need `admin`, which the token lacks.
    let resp = ui.get("/plugins", Some(&cookie)).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(resp.text().await?.contains("not allowed"));
    Ok(())
}

#[tokio::test]
async fn revoking_the_token_ends_the_session() -> Result<()> {
    let ui = Ui::start(Gate::login_required).await?;
    let token = ui.tc.create_token("phone", "reader")?;
    let cookie = ui.session(&token).await?;

    let conn = ui.tc.database_conn()?;
    conn.execute("DELETE FROM api_tokens", [])?;
    let resp = ui.get("/", Some(&cookie)).await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.text().await?.contains("Log in again"));
    Ok(())
}

#[tokio::test]
async fn logging_out_ends_the_session() -> Result<()> {
    let ui = Ui::start(Gate::login_required).await?;
    let token = ui.tc.create_token("phone", "admin")?;
    let cookie = ui.session(&token).await?;
    assert_eq!(ui.get("/", Some(&cookie)).await?.status(), StatusCode::OK);

    let resp = ui
        .browser
        .post(ui.url("/logout"))
        .header(header::COOKIE, &cookie)
        .send()
        .await?;
    assert_eq!(location(&resp), Some("/login"));
    let cleared = resp
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(cleared.contains("Max-Age=0"), "{cleared}");

    let resp = ui.get("/", Some(&cookie)).await?;
    assert_eq!(location(&resp), Some("/login"));
    Ok(())
}

#[tokio::test]
async fn the_cookie_is_secure_behind_https() -> Result<()> {
    let ui = Ui::start(Gate::login_required).await?;
    let token = ui.tc.create_token("phone", "reader")?;
    let resp = ui
        .browser
        .post(ui.url("/login"))
        .header("X-Forwarded-Proto", "https")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form_body(&token))
        .send()
        .await?;
    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(set_cookie.ends_with("; Secure"), "{set_cookie}");
    Ok(())
}

#[tokio::test]
async fn without_login_everything_is_allowed() -> Result<()> {
    let ui = Ui::start(Gate::open).await?;
    let resp = ui.get("/login", None).await?;
    assert_eq!(location(&resp), Some("/"));

    let resp = ui.get("/", None).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let page = resp.text().await?;
    assert!(
        page.contains("data-scopes=\"read state tags feeds metrics admin\""),
        "{page}"
    );
    assert!(!page.contains("class=\"logout\""), "{page}");
    Ok(())
}
