//! t3330: static bodies stream from the open file instead of being read whole. These tests check
//! the streamed bytes, the declared length, and the status and headers against the file on disk,
//! for an empty file, a file of exactly one chunk, and a file of several chunks ending in a short
//! one. A HEAD keeps the length and sends no body, and a file that shrinks mid-stream ends the
//! body with an error instead of a short body under the longer Content-Length.

use std::{fs, path::Path};

use axum::{
    body::to_bytes,
    http::{header, StatusCode},
};
use tempfile::TempDir;

use super::{file_response, read_file, STATIC_CHUNK_BYTES};

fn write(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn three_shapes() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("empty.bin", Vec::new()),
        ("exact.bin", vec![7; STATIC_CHUNK_BYTES as usize]),
        (
            "large.bin",
            (0..300_003u32).map(|index| (index % 251) as u8).collect(),
        ),
    ]
}

#[tokio::test]
async fn streamed_bodies_match_the_file_and_declare_its_length() {
    let dir = TempDir::new().unwrap();
    for (name, bytes) in three_shapes() {
        let path = write(dir.path(), name, &bytes);
        let file = read_file(&path, false).expect("the file opens for a GET");
        let response = file_response(file, StatusCode::OK);

        assert_eq!(response.status(), StatusCode::OK, "{name}: status");
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            bytes.len().to_string().as_str(),
            "{name}: Content-Length"
        );
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            super::media_type(&path),
            "{name}: Content-Type"
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the streamed body completes");
        assert!(body == bytes, "{name}: streamed bytes differ from the file");
    }
}

#[tokio::test]
async fn head_keeps_the_length_and_sends_no_body() {
    let dir = TempDir::new().unwrap();
    let (name, bytes) = ("large.bin", three_shapes().remove(2).1);
    let path = write(dir.path(), name, &bytes);
    let file = read_file(&path, true).expect("the file stats for a HEAD");
    assert!(file.handle.is_none(), "HEAD opens no body handle");

    let response = file_response(file, StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_LENGTH],
        bytes.len().to_string().as_str(),
        "HEAD Content-Length"
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(body.is_empty(), "HEAD has no body");
}

#[tokio::test]
async fn a_file_that_shrinks_while_streaming_ends_in_an_error() {
    let dir = TempDir::new().unwrap();
    let bytes = vec![3u8; 200_000];
    let path = write(dir.path(), "shrinks.bin", &bytes);
    let file = read_file(&path, false).expect("the file opens for a GET");
    // Truncate after the handle is open: the response still declares 200,000 bytes.
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(10)
        .unwrap();

    let response = file_response(file, StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_LENGTH],
        "200000",
        "the declared length is the one from the opened file"
    );
    assert!(
        to_bytes(response.into_body(), usize::MAX).await.is_err(),
        "a short file must end the body with an error"
    );
}
