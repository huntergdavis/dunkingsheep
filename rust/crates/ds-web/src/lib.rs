//! The Rust web adapter foundation. Route handlers are added in later T62 slices.

#![forbid(unsafe_code)]

/// An empty router for the web server to extend with the T62 route handlers.
pub fn router() -> axum::Router {
    axum::Router::new()
}

#[cfg(test)]
mod tests {
    #[test]
    fn foundation_router_constructs() {
        let _router = super::router();
    }
}
