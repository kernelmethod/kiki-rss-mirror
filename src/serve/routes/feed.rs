use axum::{http::StatusCode, routing::post, Json, Router};

pub fn create_router() -> Router {
    Router::new().route("/", post(add_feed))
}

#[derive(serde::Serialize)]
struct AddFeedResult {}

#[axum::debug_handler]
async fn add_feed() -> (StatusCode, Json<AddFeedResult>) {
    let result = AddFeedResult {};

    (StatusCode::CREATED, Json(result))
}
