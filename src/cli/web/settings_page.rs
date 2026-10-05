use super::layout::render_page;
use axum::{http::StatusCode, response::Response};

/// Render the settings page, which links to each part of Kiki that can be
/// set up from the web UI. For now that is only the plugins.
///
/// The page asks nothing of the Kiki server. Each section is marked
/// `admin-only`, so that the page shows it only to those whose token may
/// use what it links to.
pub(super) async fn settings_page() -> Response {
    render_page(
        StatusCode::OK,
        "Settings - Kiki",
        "<h2>Settings</h2>\n\
         <section class=\"settings-section admin-only\">\n\
         <h3><a href=\"/plugins\">Plugins</a></h3>\n\
         <p class=\"description\">See the installed plugins, and change their config.</p>\n\
         </section>\n",
    )
}
