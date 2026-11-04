//! A small error value that preserves the complete HTTP response at route boundaries.

use axum::response::{IntoResponse, Response};

#[derive(Debug)]
pub(crate) struct WebResponseError(Box<Response>);

impl From<Response> for WebResponseError {
    fn from(response: Response) -> Self {
        Self(Box::new(response))
    }
}

impl IntoResponse for WebResponseError {
    fn into_response(self) -> Response {
        *self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::{header, HeaderValue, StatusCode, Version},
    };
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };

    #[test]
    fn boxed_response_round_trip_preserves_status_headers_and_body() {
        let bytes = b"\x00HTTP error bytes\xff\r\n";
        let mut response = Response::new(Body::from(bytes.as_slice()));
        *response.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
        *response.version_mut() = Version::HTTP_2;
        response.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_static("first=1; HttpOnly"),
        );
        response.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_static("second=2; Secure"),
        );
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        response.extensions_mut().insert(42_u64);
        let headers = response.headers().clone();

        let restored = WebResponseError::from(response).into_response();
        assert_eq!(restored.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(restored.version(), Version::HTTP_2);
        assert_eq!(restored.headers(), &headers);
        assert_eq!(
            restored
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .count(),
            2
        );
        assert_eq!(restored.extensions().get::<u64>(), Some(&42));
        let mut body = std::pin::pin!(to_bytes(restored.into_body(), usize::MAX));
        let mut context = Context::from_waker(Waker::noop());
        let Poll::Ready(body) = body.as_mut().poll(&mut context) else {
            panic!("in-memory body should be immediately available");
        };
        assert_eq!(body.unwrap().as_ref(), bytes);
    }
}
