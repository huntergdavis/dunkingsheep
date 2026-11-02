//! The Rust web adapter foundation.

#![forbid(unsafe_code)]

pub mod routes;

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
