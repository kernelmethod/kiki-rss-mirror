use super::*;
use crate::auth::Scope;
use crate::test::TestBuilder;
use anyhow::Result;
use axum::http::StatusCode;
use serde_json::json;

#[tokio::test]
async fn create_list_use_and_revoke() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;

    let resp = client
        .post("http://localhost/v1/tokens")
        .json(&json!({ "name": "phone", "scopes": ["reader", "metrics"] }))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created: CreateTokenResponse = resp.json().await?;
    assert_eq!(created.details.name, "phone");
    assert_eq!(created.details.scopes.to_string(), "read,state,metrics");
    assert_eq!(created.details.expires_at, None);
    assert!(created.token.starts_with("kiki_"));

    let list: ListTokensResponse = client
        .get("http://localhost/v1/tokens")
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(list.tokens, vec![created.details.clone()]);
    // The secret is never listed.
    let raw = client
        .get("http://localhost/v1/tokens")
        .send()
        .await?
        .text()
        .await?;
    assert!(!raw.contains(&created.token));
    assert!(!raw.contains("secret"));

    let current: CurrentTokenResponse = client
        .get("http://localhost/v1/tokens/current")
        .bearer_auth(&created.token)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(current.token.map(|t| t.id), Some(created.details.id));
    assert!(current.scopes.contains(Scope::Metrics));
    assert!(!current.scopes.contains(Scope::Tags));

    let resp = client
        .delete(format!(
            "http://localhost/v1/tokens/id/{}",
            created.details.id
        ))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = client
        .get("http://localhost/v1/tokens/current")
        .bearer_auth(&created.token)
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = client
        .delete(format!(
            "http://localhost/v1/tokens/id/{}",
            created.details.id
        ))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn current_without_a_token_on_the_socket() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let current: CurrentTokenResponse = tc
        .client()?
        .get("http://localhost/v1/tokens/current")
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(current.token, None);
    assert_eq!(current.scopes, Scopes::all());
    Ok(())
}

#[tokio::test]
async fn create_rejects_bad_requests() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;
    let create =
        |body: serde_json::Value| client.post("http://localhost/v1/tokens").json(&body).send();

    let resp = create(json!({ "name": "a", "scopes": ["read"] })).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = create(json!({ "name": "a", "scopes": ["read"] })).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    let resp = create(json!({ "name": "b", "scopes": [] })).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = create(json!({ "name": "", "scopes": ["read"] })).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = create(json!({ "name": "c", "scopes": ["root"] })).await?;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let resp = create(json!({ "name": "d", "scopes": ["read"], "expires_at": 1 })).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let future = chrono::Utc::now().timestamp() + 3600;
    let resp = create(json!({ "name": "e", "scopes": ["read"], "expires_at": future })).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created: CreateTokenResponse = resp.json().await?;
    assert_eq!(created.details.expires_at, Some(future));
    Ok(())
}
