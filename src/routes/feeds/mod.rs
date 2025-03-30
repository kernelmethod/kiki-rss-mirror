mod add_feed;
mod delete_feed;
mod get_feed;
mod list_feeds;

use add_feed::add_feed;
use delete_feed::delete_feed;
use get_feed::get_feed;
use list_feeds::list_feeds;

use crate::server::AppState;
use axum::{routing::get, Router};

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_feeds).post(add_feed))
        .route("/{*id}", get(get_feed).delete(delete_feed))
}

#[cfg(test)]
mod test {}
