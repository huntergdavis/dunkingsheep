//! The Rust web adapter foundation.

#![forbid(unsafe_code)]

pub mod auth_backend;
pub mod dashboard_activity_backend;
pub mod http_adapter;
pub mod messages_backend;
pub mod ops_board_runtime;
pub mod ops_collector;
pub mod ops_history;
pub mod ops_summary;
pub mod owner_fleet_backend;
pub mod panes_backend;
pub mod python_http;
pub mod python_json;
pub mod roster;
pub mod routes;
pub mod terminal_stream;
pub mod terminal_stream_route;
pub mod web_server;

pub use routes::auth::RequestScheme;

/// Router for the web server's T62 route handlers, configured for plain HTTP.
/// Use [`router_with_scheme`] when TLS or a trusted reverse proxy exposes HTTPS.
pub fn router() -> axum::Router {
    routes::auth::router()
}

/// Router configured with the trusted externally visible request scheme.
pub fn router_with_scheme(scheme: RequestScheme) -> axum::Router {
    routes::auth::router_with_scheme(scheme)
}

#[cfg(test)]
mod tests {
    #[test]
    fn foundation_router_constructs() {
        let _router = super::router();
    }

    #[test]
    fn foundation_router_accepts_explicit_https_scheme() {
        let _router = super::router_with_scheme(super::RequestScheme::Https);
    }
}
