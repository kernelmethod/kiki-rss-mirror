//! Logging in to the web UI with an API token.
//!
//! With `--require-login` (or `web_ui.require_login`), every page asks
//! for an API token first. The web UI checks the token with the Kiki API,
//! keeps it in a session held in memory, and sends it with every request
//! it makes to the API for that session, so the API holds the session to
//! the token's scopes. Pages hide the controls the token cannot use.
//!
//! The session cookie is `HttpOnly` and `SameSite=Strict`, so other sites
//! can neither read it nor have the browser send it to the web UI.
//! Sessions do not survive a restart of the web UI.
//!
//! Without login, the web UI acts with the full access its Unix socket
//! gives it, as it always has.

use super::layout::render_form_page;
use super::plugins::is_same_origin;
use super::API_BASE;
use crate::auth::Scopes;
use crate::routes::v1::tokens::CurrentTokenResponse;
use axum::{
    extract::{FromRequestParts, Request, State},
    http::{header, request::Parts, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    Form,
};
use base64::Engine;
use quick_xml::escape::escape;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The name of the session cookie.
pub(super) const SESSION_COOKIE: &str = "kiki_session";

/// How long a session lasts, unless the token it holds expires or is
/// revoked first.
const SESSION_LIFETIME: Duration = Duration::from_secs(30 * 86400);

/// The most sessions kept at once. Logging in past this ends the session
/// closest to expiring.
const MAX_SESSIONS: usize = 1024;

/// How long a failed login waits before answering, to slow down guessing.
/// Tokens are far too long to guess anyway.
const FAILED_LOGIN_DELAY: Duration = Duration::from_millis(500);

/// A client for the Kiki API that acts for one session: it sends the
/// session's token, if it has one, with every request.
#[derive(Clone)]
pub(super) struct Api {
    client: reqwest::Client,
    token: Option<Arc<str>>,
}

impl Api {
    /// A client acting with the full access of the Unix socket.
    pub(super) fn unauthenticated(client: reqwest::Client) -> Self {
        Api {
            client,
            token: None,
        }
    }

    fn request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        let req = self.client.request(method, url);
        match &self.token {
            Some(token) => req.header(header::AUTHORIZATION, bearer(token)),
            None => req,
        }
    }

    pub(super) fn get(&self, url: String) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::GET, url)
    }

    pub(super) fn post(&self, url: String) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::POST, url)
    }

    pub(super) fn put(&self, url: String) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::PUT, url)
    }

    pub(super) fn patch(&self, url: String) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::PATCH, url)
    }

    pub(super) fn delete(&self, url: String) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::DELETE, url)
    }
}

/// The `Authorization` header value for `token`, marked sensitive so that
/// it is never logged.
fn bearer(token: &str) -> HeaderValue {
    let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
        .unwrap_or_else(|_| HeaderValue::from_static("Bearer invalid"));
    value.set_sensitive(true);
    value
}

/// The [`Api`] that [`gate`] chose for the request.
impl<S: Send + Sync> FromRequestParts<S> for Api {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<Api>().cloned().ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "The request reached a page without passing the login check.",
        ))
    }
}

/// Who a page is being rendered for, so that it can show what they may do.
#[derive(Clone, Debug)]
pub(super) struct Viewer {
    /// What the viewer may do.
    pub(super) scopes: Scopes,
    /// The name of the token the viewer logged in with, if they did.
    pub(super) token_name: Option<String>,
}

tokio::task_local! {
    static VIEWER: Viewer;
}

/// The viewer of the page being rendered: whoever logged in, or, without
/// login, someone who may do anything.
pub(super) fn current_viewer() -> Viewer {
    VIEWER.try_with(Viewer::clone).unwrap_or(Viewer {
        scopes: Scopes::all(),
        token_name: None,
    })
}

/// A logged-in session.
struct Session {
    token: Arc<str>,
    viewer: Viewer,
    expires: Instant,
}

/// The sessions of the people logged in to the web UI.
#[derive(Default)]
pub(super) struct Sessions(Mutex<HashMap<String, Session>>);

impl Sessions {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        // A panic while the lock was held leaves nothing half-done: every
        // change to the map is a single insert or remove.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Start a session for `token`, and return its id.
    fn start(&self, token: &str, viewer: Viewer) -> Result<String, getrandom::Error> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)?;
        let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let now = Instant::now();
        let mut sessions = self.lock();
        sessions.retain(|_, s| s.expires > now);
        if sessions.len() >= MAX_SESSIONS {
            let oldest = sessions
                .iter()
                .min_by_key(|(_, s)| s.expires)
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(
            id.clone(),
            Session {
                token: token.into(),
                viewer,
                expires: now + SESSION_LIFETIME,
            },
        );
        Ok(id)
    }

    /// The token and viewer of the live session `id`.
    fn get(&self, id: &str) -> Option<(Arc<str>, Viewer)> {
        let mut sessions = self.lock();
        match sessions.get(id) {
            Some(s) if s.expires > Instant::now() => Some((s.token.clone(), s.viewer.clone())),
            Some(_) => {
                sessions.remove(id);
                None
            }
            None => None,
        }
    }

    /// End session `id`.
    fn end(&self, id: &str) {
        self.lock().remove(id);
    }
}

/// How the web UI decides who may use it.
pub(super) struct Gate {
    /// The client for the Kiki API, without a token.
    api: reqwest::Client,
    /// The sessions of those logged in, or `None` if login is not
    /// required.
    sessions: Option<Sessions>,
}

impl Gate {
    /// Let everyone use the web UI with the full access of `api`.
    pub(super) fn open(api: reqwest::Client) -> Self {
        Gate {
            api,
            sessions: None,
        }
    }

    /// Require everyone to log in with an API token, which is then sent
    /// with each of their requests through `api`.
    pub(super) fn login_required(api: reqwest::Client) -> Self {
        Gate {
            api,
            sessions: Some(Sessions::default()),
        }
    }
}

/// The value of the session cookie in `headers`, if there is one.
fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value)
}

/// Whether the request reached the web UI over HTTPS, as a reverse proxy
/// in front of it reports, so that the cookie can be marked `Secure`.
fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("https"))
}

/// A `Set-Cookie` header setting the session cookie to `value` for
/// `max_age` seconds; a `max_age` of 0 clears it.
fn set_cookie(value: &str, max_age: u64, secure: bool) -> (header::HeaderName, String) {
    let secure = if secure { "; Secure" } else { "" };
    (
        header::SET_COOKIE,
        format!(
            "{SESSION_COOKIE}={value}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Strict{secure}"
        ),
    )
}

/// Middleware that lets a request through to a page only once its sender
/// has logged in, when login is required, and gives the page the [`Api`]
/// to act through. Requests for pages without a session are sent to the
/// login page; other requests are refused.
pub(super) async fn gate(State(gate): State<Arc<Gate>>, mut req: Request, next: Next) -> Response {
    let Some(sessions) = &gate.sessions else {
        req.extensions_mut()
            .insert(Api::unauthenticated(gate.api.clone()));
        return next.run(req).await;
    };
    if matches!(req.uri().path(), "/login" | "/logout") {
        return next.run(req).await;
    }
    let session = session_cookie(req.headers()).and_then(|id| sessions.get(id));
    let Some((token, viewer)) = session else {
        return if matches!(*req.method(), Method::GET | Method::HEAD) {
            Redirect::to("/login").into_response()
        } else {
            (StatusCode::UNAUTHORIZED, "Log in to the web UI first.").into_response()
        };
    };
    req.extensions_mut().insert(Api {
        client: gate.api.clone(),
        token: Some(token),
    });
    VIEWER.scope(viewer, next.run(req)).await
}

/// Render the login page, with `error` above the form if there is one.
fn login_page(status: StatusCode, error: Option<&str>) -> Response {
    let error = error
        .map(|e| {
            format!(
                "<p class=\"login-error\" role=\"alert\">{}</p>\n",
                escape(e)
            )
        })
        .unwrap_or_default();
    let content = format!(
        "<h2>Log in</h2>\n{error}<form class=\"login\" method=\"post\" action=\"/login\">\n\
         <label for=\"token\">API token</label>\n\
         <input id=\"token\" name=\"token\" type=\"password\" autocomplete=\"current-password\" \
         required autofocus>\n\
         <button type=\"submit\">Log in</button>\n\
         <span class=\"hint\">Create a token with <code>kiki token create</code>.</span>\n\
         </form>\n"
    );
    // Rendered for nobody in particular, so the page offers nothing but
    // the form.
    let nobody = Viewer {
        scopes: Scopes::NONE,
        token_name: None,
    };
    let mut resp = VIEWER.sync_scope(nobody, || {
        render_form_page(status, "Log in - Kiki", &content)
    });
    // Keep the page and the token typed into it out of caches.
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// Show the login page, or go home if login is not required.
pub(super) async fn show_login(State(gate): State<Arc<Gate>>) -> Response {
    if gate.sessions.is_none() {
        return Redirect::to("/").into_response();
    }
    login_page(StatusCode::OK, None)
}

/// The login form.
#[derive(Deserialize)]
pub(super) struct LoginForm {
    token: String,
}

/// Log in with the token in `form`: check it with the Kiki API, start a
/// session for it, and go home.
pub(super) async fn log_in(
    State(gate): State<Arc<Gate>>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let Some(sessions) = &gate.sessions else {
        return Redirect::to("/").into_response();
    };
    // Otherwise another site could log the browser in with a token of its
    // choosing.
    if !is_same_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            "You can only log in from the web UI's own login page.",
        )
            .into_response();
    }
    let token = form.token.trim();
    let resp = gate
        .api
        .get(format!("{API_BASE}/v1/tokens/current"))
        .header(header::AUTHORIZATION, bearer(token))
        .send()
        .await;
    let current = match resp {
        Ok(resp) if resp.status() == StatusCode::OK => resp.json::<CurrentTokenResponse>().await,
        Ok(resp) if resp.status() == StatusCode::UNAUTHORIZED => {
            tokio::time::sleep(FAILED_LOGIN_DELAY).await;
            return login_page(
                StatusCode::UNAUTHORIZED,
                Some("That token is not valid, or has expired or been revoked."),
            );
        }
        Ok(resp) => {
            tracing::warn!(status = %resp.status(), "unexpected answer checking a token");
            return login_page(
                StatusCode::BAD_GATEWAY,
                Some("The Kiki server could not check the token."),
            );
        }
        Err(e) => {
            tracing::warn!("failed to reach the Kiki server: {e:#}");
            return login_page(
                StatusCode::BAD_GATEWAY,
                Some("The Kiki server is unavailable."),
            );
        }
    };
    let current = match current {
        Ok(current) => current,
        Err(e) => {
            tracing::warn!("unexpected answer checking a token: {e:#}");
            return login_page(
                StatusCode::BAD_GATEWAY,
                Some("The Kiki server could not check the token."),
            );
        }
    };
    let viewer = Viewer {
        scopes: current.scopes,
        token_name: current.token.map(|t| t.name),
    };
    tracing::info!(
        token = viewer.token_name.as_deref().unwrap_or(""),
        "logged in to the web UI"
    );
    match sessions.start(token, viewer) {
        Ok(id) => (
            [set_cookie(
                &id,
                SESSION_LIFETIME.as_secs(),
                is_https(&headers),
            )],
            Redirect::to("/"),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("unable to start a session: {e}");
            login_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                Some("The web UI could not start a session."),
            )
        }
    }
}

/// End the session, if there is one, and go to the login page.
pub(super) async fn log_out(State(gate): State<Arc<Gate>>, headers: HeaderMap) -> Response {
    if !is_same_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            "You can only log out from the web UI's own pages.",
        )
            .into_response();
    }
    if let (Some(sessions), Some(id)) = (&gate.sessions, session_cookie(&headers)) {
        sessions.end(id);
    }
    (
        [set_cookie("", 0, is_https(&headers))],
        Redirect::to("/login"),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_session_cookie() {
        let mut headers = HeaderMap::new();
        assert_eq!(session_cookie(&headers), None);
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; kiki_session=abc; b=2"),
        );
        assert_eq!(session_cookie(&headers), Some("abc"));
    }

    #[test]
    fn sessions_start_and_end() -> Result<(), getrandom::Error> {
        let sessions = Sessions::default();
        let viewer = Viewer {
            scopes: Scopes::NONE,
            token_name: Some("t".into()),
        };
        let a = sessions.start("token-a", viewer.clone())?;
        let b = sessions.start("token-b", viewer)?;
        assert_ne!(a, b);
        assert_eq!(sessions.get(&a).map(|(t, _)| t), Some("token-a".into()));
        sessions.end(&a);
        assert!(sessions.get(&a).is_none());
        assert!(sessions.get(&b).is_some());
        assert!(sessions.get("forged").is_none());
        Ok(())
    }

    #[test]
    fn sessions_are_capped() -> Result<(), getrandom::Error> {
        let sessions = Sessions::default();
        let viewer = Viewer {
            scopes: Scopes::NONE,
            token_name: None,
        };
        let first = sessions.start("t", viewer.clone())?;
        for _ in 0..MAX_SESSIONS {
            sessions.start("t", viewer.clone())?;
        }
        assert_eq!(sessions.lock().len(), MAX_SESSIONS);
        assert!(sessions.get(&first).is_none());
        Ok(())
    }
}
