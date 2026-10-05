//! End-to-end tests of token checks over the Unix socket.

use crate::db::tokens;
use crate::test::{TestBuilder, TestConfig};
use anyhow::Result;
use axum::http::StatusCode;
use std::time::{Duration, Instant};

/// A feed with one entry, id 1, and a user tag named `news`.
fn populate(tc: &TestConfig) -> Result<()> {
    let conn = tc.database_conn()?;
    conn.execute_batch(
        "INSERT INTO feeds (title, url, syndication_format) VALUES ('Feed', 'http://f/', 'rss');
         INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
         VALUES (1, 'rss', 'g', 1700000000, 'Entry', 'http://f/1');
         INSERT INTO tags (name) VALUES ('news');",
    )?;
    Ok(())
}

fn tag_id(tc: &TestConfig, name: &str) -> Result<i64> {
    Ok(tc
        .database_conn()?
        .query_row("SELECT id FROM tags WHERE name = ?1", [name], |r| r.get(0))?)
}

#[tokio::test]
async fn requests_without_a_token_may_do_anything() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;
    for path in ["/v1/feeds", "/v1/tokens", "/v1/plugins", "/metrics"] {
        let resp = client.get(format!("http://localhost{path}")).send().await?;
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
    }
    Ok(())
}

#[tokio::test]
async fn bad_tokens_are_refused() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let token = tc.create_token("t", "admin")?;
    let client = tc.client()?;

    let forged = format!("{token}x");
    for auth in [
        format!("Bearer {forged}"),
        "Bearer nonsense".to_owned(),
        format!("Basic {token}"),
        token.clone(),
    ] {
        // A bad token is refused rather than ignored, even on routes that
        // need no scope.
        for path in ["/v1/feeds", "/v1/health"] {
            let resp = client
                .get(format!("http://localhost{path}"))
                .header("authorization", &auth)
                .send()
                .await?;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{auth} {path}");
            assert_eq!(
                resp.headers()
                    .get("www-authenticate")
                    .and_then(|v| v.to_str().ok()),
                Some("Bearer realm=\"kiki\"")
            );
        }
    }

    // The scheme is case-insensitive.
    let resp = client
        .get("http://localhost/v1/feeds")
        .header("authorization", format!("bearer {token}"))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn expired_and_revoked_tokens_are_refused() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;

    let conn = tc.database_conn()?;
    let (_, expired) = tokens::create(&conn, "old", "read".parse()?, Some(1))?;
    let resp = client
        .get("http://localhost/v1/feeds")
        .bearer_auth(&expired)
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(resp.text().await?, "This token has expired");

    let (revoked, token) = tokens::create(&conn, "gone", "read".parse()?, None)?;
    let resp = client
        .get("http://localhost/v1/feeds")
        .bearer_auth(&token)
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    tokens::revoke(&conn, revoked.id)?;
    let resp = client
        .get("http://localhost/v1/feeds")
        .bearer_auth(&token)
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn scopes_decide_what_a_token_may_do() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    populate(&tc)?;
    let client = tc.client()?;
    let url = "http://localhost";
    let read = tc.create_token("read", "read")?;
    let reader = tc.create_token("reader", "reader")?;
    let curator = tc.create_token("curator", "curator")?;
    let metrics = tc.create_token("metrics", "metrics")?;

    let status = |req: reqwest::RequestBuilder| async move {
        Ok::<_, anyhow::Error>(req.send().await?.status())
    };

    // Routes that need no scope.
    for path in ["/v1/health", "/v1/", "/docs", "/v1/tokens/current"] {
        let s = status(client.get(format!("{url}{path}")).bearer_auth(&metrics)).await?;
        assert_eq!(s, StatusCode::OK, "{path}");
    }

    // Reading.
    for token in [&read, &reader, &curator] {
        let s = status(client.get(format!("{url}/v1/feeds")).bearer_auth(token)).await?;
        assert_eq!(s, StatusCode::OK);
        let s = status(
            client
                .post(format!("{url}/v1/entries/search"))
                .bearer_auth(token)
                .json(&serde_json::json!({})),
        )
        .await?;
        assert_eq!(s, StatusCode::OK);
    }
    let s = status(client.get(format!("{url}/v1/feeds")).bearer_auth(&metrics)).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Marking an entry read.
    let mark = |token: &str| {
        client
            .put(format!("{url}/v1/entries/id/1/system-tags/system:read"))
            .bearer_auth(token)
    };
    assert_eq!(status(mark(&read)).await?, StatusCode::FORBIDDEN);
    assert_eq!(status(mark(&reader)).await?, StatusCode::OK);
    assert_eq!(status(mark(&curator)).await?, StatusCode::OK);

    // Creating tags.
    let create = |token: &str, name: &str| {
        client
            .post(format!("{url}/v1/tags/create"))
            .bearer_auth(token)
            .json(&serde_json::json!({ "name": name }))
    };
    let resp = create(&reader, "a").send().await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(resp.text().await?, "This token lacks the tags scope");
    assert_eq!(status(create(&curator, "b")).await?, StatusCode::CREATED);

    // Bulk tagging: system tags need `state`, user tags `tags`.
    let saved = tag_id(&tc, "system:saved")?;
    let news = tag_id(&tc, "news")?;
    let bulk = |token: &str, tag: i64| {
        client
            .post(format!("{url}/v1/tags/id/{tag}/entries"))
            .bearer_auth(token)
            .json(&serde_json::json!({ "entry_ids": [1] }))
    };
    assert_eq!(status(bulk(&read, saved)).await?, StatusCode::FORBIDDEN);
    assert_eq!(status(bulk(&reader, saved)).await?, StatusCode::OK);
    assert_eq!(status(bulk(&reader, news)).await?, StatusCode::FORBIDDEN);
    assert_eq!(status(bulk(&curator, news)).await?, StatusCode::OK);
    let s = status(
        client
            .delete(format!("{url}/v1/tags/id/{news}/entries"))
            .bearer_auth(&reader)
            .json(&serde_json::json!({ "entry_ids": [1] })),
    )
    .await?;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Feeds, settings, plugins, tokens and shutdown.
    for token in [&reader, &curator] {
        let s = status(
            client
                .delete(format!("{url}/v1/feeds/id/1"))
                .bearer_auth(token),
        )
        .await?;
        assert_eq!(s, StatusCode::FORBIDDEN);
        for path in ["/v1/settings/retention", "/v1/plugins", "/v1/tokens"] {
            let s = status(client.get(format!("{url}{path}")).bearer_auth(token)).await?;
            assert_eq!(s, StatusCode::FORBIDDEN, "{path}");
        }
        let s = status(client.post(format!("{url}/v1/shutdown")).bearer_auth(token)).await?;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }

    // Metrics.
    let s = status(client.get(format!("{url}/metrics")).bearer_auth(&reader)).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let s = status(client.get(format!("{url}/metrics")).bearer_auth(&metrics)).await?;
    assert_eq!(s, StatusCode::OK);

    // A route that doesn't exist is a 404 for any token.
    let s = status(client.get(format!("{url}/v1/nope")).bearer_auth(&metrics)).await?;
    assert_eq!(s, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn admin_tokens_may_do_anything() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    populate(&tc)?;
    let client = tc.client()?;
    let admin = tc.create_token("admin", "admin")?;

    for path in [
        "/v1/feeds",
        "/v1/settings/retention",
        "/v1/plugins",
        "/v1/tokens",
        "/metrics",
    ] {
        let resp = client
            .get(format!("http://localhost{path}"))
            .bearer_auth(&admin)
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
    }
    let resp = client
        .delete("http://localhost/v1/feeds/id/1")
        .bearer_auth(&admin)
        .send()
        .await?;
    assert!(resp.status().is_success(), "{}", resp.status());
    Ok(())
}

#[tokio::test]
async fn token_use_is_recorded() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let token = tc.create_token("t", "read")?;
    let id = crate::auth::PresentedToken::parse(&token)
        .map(|t| t.id)
        .ok_or_else(|| anyhow::anyhow!("bad token"))?;

    let resp = tc
        .client()?
        .get("http://localhost/v1/feeds")
        .bearer_auth(&token)
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    // The use is recorded in the background.
    let start = Instant::now();
    loop {
        let conn = tc.database_conn()?;
        if tokens::get(&conn, id)?
            .and_then(|t| t.last_used_at)
            .is_some()
        {
            return Ok(());
        }
        if start.elapsed() > Duration::from_secs(5) {
            anyhow::bail!("token use was not recorded");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
