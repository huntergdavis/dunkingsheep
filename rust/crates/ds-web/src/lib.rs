//! The Rust web adapter foundation.

#![forbid(unsafe_code)]

pub mod routes;

/// Router for the web server's T62 route handlers.
pub fn router() -> axum::Router {
    routes::auth::router()
}

#[cfg(test)]
mod tests {
    #[test]
    fn foundation_router_constructs() {
        let _router = super::router();
    }
}
