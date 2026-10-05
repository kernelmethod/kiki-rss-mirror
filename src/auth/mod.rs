//! API tokens, and checking that a request may do what it asks.
//!
//! The API is served on a Unix socket, and optionally on a TCP listener
//! (`kiki serve --listen`). A request presents a token with an
//! `Authorization: Bearer <token>` header, and the token's [`Scopes`]
//! decide which routes it may use; see [`policy::requirement`].
//!
//! On the Unix socket a token is optional. Whoever can open the socket
//! already has the run of the data directory, so a request without one is
//! allowed everything, as it always has been. A request that does carry a
//! token is held to that token's scopes, which is how the web UI acts for
//! someone who logged in with a token. On the TCP listener every request
//! needs a token, except to the few [`Requirement::Public`] routes.

pub mod policy;
mod scope;
mod token;

pub use policy::Requirement;
pub use scope::{Scope, Scopes, UnknownScope};
pub use token::{PresentedToken, Secret, TOKEN_PREFIX};

use crate::db::tokens::{self, Authentication, Token};
use crate::server::AppState;
use axum::{
    extract::{FromRequestParts, MatchedPath, Request, State},
    http::{header, request::Parts, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::convert::Infallible;

/// Which listener a request arrived on. Set as a request extension by the
/// server; a request without one is treated as [`Transport::Network`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// The Unix socket, where requests need no token.
    Socket,
    /// The TCP listener, where requests need a token.
    Network,
}

/// Who a request is from, as decided by [`authorize`], which adds it to
/// every request it lets through. Handlers that need finer checks than
/// their route's [`Requirement`] can extract it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// A client on the Unix socket that presented no token, which may do
    /// anything.
    Socket,
    /// A client that presented no token for a public route.
    Anonymous,
    /// A client that presented a valid token.
    Token(Token),
}

impl Principal {
    /// The scopes the principal holds.
    pub fn scopes(&self) -> Scopes {
        match self {
            Principal::Socket => Scopes::all(),
            Principal::Anonymous => Scopes::NONE,
            Principal::Token(token) => token.scopes,
        }
    }

    /// Whether the principal holds `scope`.
    pub fn allows(&self, scope: Scope) -> bool {
        self.scopes().contains(scope)
    }

    fn satisfies(&self, requirement: Requirement) -> bool {
        match requirement {
            Requirement::Public => true,
            Requirement::Authenticated => !matches!(self, Principal::Anonymous),
            Requirement::Scope(scope) => self.allows(scope),
        }
    }
}

/// A principal holding no scopes, for a request [`authorize`] never saw,
/// so that a handler checking scopes refuses it.
impl<S: Send + Sync> FromRequestParts<S> for Principal {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<Principal>()
            .cloned()
            .unwrap_or(Principal::Anonymous))
    }
}

/// The response to a request that needs a scope its token lacks.
pub fn forbidden(scope: Scope) -> Response {
    (
        StatusCode::FORBIDDEN,
        format!("This token lacks the {scope} scope"),
    )
        .into_response()
}

/// The response to a request with a missing, invalid or expired token.
fn unauthorized(message: &'static str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"kiki\""),
        )],
        message,
    )
        .into_response()
}

/// The token in the request's `Authorization` header: `Ok(None)` if there
/// is no such header, and `Err` if it is not a bearer token.
fn bearer_token(req: &Request) -> Result<Option<&str>, ()> {
    let Some(value) = req.headers().get(header::AUTHORIZATION) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| ())?;
    let (scheme, token) = value.trim().split_once(' ').ok_or(())?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(());
    }
    Ok(Some(token.trim()))
}

/// Middleware that lets a request through only if it may use the route it
/// asks for, and adds its [`Principal`] to it.
///
/// It must wrap each route, as [`axum::Router::layer`] does, so that it
/// sees the route's [`MatchedPath`]. A request matching no route needs only
/// to be [`Requirement::Authenticated`], so that the API's 404s are not
/// shown to clients without a token.
pub async fn authorize(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let transport = req
        .extensions()
        .get::<Transport>()
        .copied()
        .unwrap_or(Transport::Network);
    let requirement = match req.extensions().get::<MatchedPath>() {
        Some(path) => policy::requirement(req.method(), path.as_str())
            .unwrap_or(Requirement::Scope(Scope::Admin)),
        None => Requirement::Authenticated,
    };

    let principal = match bearer_token(&req) {
        Err(()) => return unauthorized("The Authorization header must hold a bearer token"),
        Ok(Some(presented)) => {
            let presented = presented.to_owned();
            let now = chrono::Utc::now().timestamp();
            let checked = state
                .db
                .read(move |conn| tokens::authenticate(conn, &presented, now))
                .await;
            match checked {
                Ok(Ok(Authentication::Valid(token))) => {
                    if tokens::needs_touch(&token, now) {
                        let db = state.db.clone();
                        let id = token.id;
                        tokio::spawn(async move {
                            if let Ok(Err(e)) =
                                db.write(move |conn| tokens::touch(conn, id, now)).await
                            {
                                tracing::warn!(token = id, "unable to record token use: {e}");
                            }
                        });
                    }
                    Principal::Token(token)
                }
                Ok(Ok(Authentication::Expired)) => return unauthorized("This token has expired"),
                Ok(Ok(Authentication::Invalid)) => return unauthorized("Invalid token"),
                Ok(Err(e)) => {
                    tracing::error!("unable to check a token: {e}");
                    return (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
                        .into_response();
                }
                Err(e) => {
                    tracing::error!("unable to check a token: {e}");
                    return (StatusCode::SERVICE_UNAVAILABLE, "Service unavailable")
                        .into_response();
                }
            }
        }
        Ok(None) => match transport {
            Transport::Socket => Principal::Socket,
            Transport::Network => Principal::Anonymous,
        },
    };

    if !principal.satisfies(requirement) {
        return match (requirement, &principal) {
            (_, Principal::Anonymous) => unauthorized("A token is required"),
            (Requirement::Scope(scope), _) => forbidden(scope),
            _ => unauthorized("A token is required"),
        };
    }

    req.extensions_mut().insert(principal);
    next.run(req).await
}

#[cfg(test)]
mod tests;
