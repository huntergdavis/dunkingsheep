//! Small HTTP/1.x compatibility listener for the legacy `BaseHTTPRequestHandler`.
//!
//! Hyper rejects some byte streams before the Python handler would see them
//! (notably duplicate lengths and a short body at EOF). This listener performs
//! only the framing decisions made by Python's standard library, then hands a
//! single already-framed request to the shared Axum route adapter.

use std::{collections::HashSet, io};

use axum::{
    body::Body,
    http::{Request, Response, StatusCode},
    Router,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
use tower::ServiceExt;

const MAX_LINE: usize = 65_536;
const MAX_HEADERS: usize = 100;

pub async fn serve(listener: TcpListener, app: Router) -> io::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let app = app.clone();
        tokio::spawn(async move {
            let _ = serve_connection(stream, peer, app).await;
        });
    }
}

pub async fn serve_with_shutdown<F>(
    listener: TcpListener,
    app: Router,
    shutdown: F,
) -> io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                let app = app.clone();
                tokio::spawn(async move { let _ = serve_connection(stream, peer, app).await; });
            }
        }
    }
}

async fn serve_connection(
    stream: TcpStream,
    peer: std::net::SocketAddr,
    app: Router,
) -> io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = Vec::new();
    match read_line_bounded(&mut reader, &mut line, MAX_LINE).await {
        Ok(false) => return Ok(()),
        Ok(true) => {}
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            return write_error(&mut write, 414, "URI Too Long", "URI Too Long").await;
        }
        Err(error) => return Err(error),
    }
    let request_line = trim_crlf(&line);
    let mut words = request_line.split_ascii_whitespace();
    let Some(method) = words.next() else {
        return write_error(&mut write, 400, "Bad Request", "bad request syntax").await;
    };
    let Some(target) = words.next() else {
        return write_error(&mut write, 400, "Bad Request", "bad request syntax").await;
    };
    let Some(version) = words.next() else {
        return write_error(&mut write, 400, "Bad Request", "bad request syntax").await;
    };
    if words.next().is_some() || !(version == "HTTP/1.0" || version == "HTTP/1.1") {
        return write_error(&mut write, 400, "Bad Request", "bad request syntax").await;
    }

    let mut headers = Vec::<(String, String)>::new();
    let mut header_count = 0;
    loop {
        line.clear();
        match read_line_bounded(&mut reader, &mut line, MAX_LINE).await {
            Ok(false) => return Ok(()),
            Ok(true) => {}
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                return write_error(
                    &mut write,
                    431,
                    "Request Header Fields Too Large",
                    "Line too long",
                )
                .await;
            }
            Err(error) => return Err(error),
        }
        if line == b"\r\n" || line == b"\n" {
            break;
        }
        header_count += 1;
        if header_count > MAX_HEADERS {
            return write_error(
                &mut write,
                431,
                "Request Header Fields Too Large",
                "Too many headers",
            )
            .await;
        }
        let raw = trim_crlf(&line);
        if let Some((name, value)) = raw.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim_matches([' ', '\t']).to_owned();
            headers.push((name, value));
        }
    }

    // `HTTPMessage.get` returns the first occurrence. The Python handler ignores
    // Transfer-Encoding entirely and reads zero bytes when Content-Length is absent.
    if method != "GET" && method != "POST" {
        return write_error(
            &mut write,
            501,
            "Not Implemented",
            &format!("Unsupported method ({})", python_repr(method)),
        )
        .await;
    }
    let content_length = if method == "POST" {
        headers.iter().find(|(name, _)| name == "content-length")
    } else {
        None
    };
    let mut body = Vec::new();
    let mut negative_length = false;
    if let Some((_, length)) = content_length {
        let (negative, zero, too_large) = match python_content_length(length) {
            Some(parsed) => parsed,
            None => {
                let message = format!(
                    "internal error: invalid literal for int() with base 10: {}",
                    python_repr(length)
                );
                return write_json_error(&mut write, 500, &message).await;
            }
        };
        if !negative && !zero && !too_large {
            let parsed = length
                .trim()
                .trim_start_matches('+')
                .replace('_', "")
                .parse::<u64>()
                .unwrap_or(0);
            let mut limited = (&mut reader).take(parsed);
            limited.read_to_end(&mut body).await?;
        } else if negative {
            negative_length = true;
            // This matches Python `rfile.read(negative)`: consume through EOF.
            reader.read_to_end(&mut body).await?;
        }
    }

    let http_version = if version == "HTTP/1.0" {
        axum::http::Version::HTTP_10
    } else {
        axum::http::Version::HTTP_11
    };
    let mut builder = Request::builder()
        .method(method)
        .uri(target)
        .version(http_version);
    let mut seen_headers = HashSet::new();
    for (name, value) in headers {
        // Keep only the first value for duplicate headers, matching Python's
        // `HTTPMessage.get`; duplicate Content-Length normalization is forbidden.
        if !seen_headers.insert(name.clone()) {
            continue;
        }
        builder = builder.header(name, value);
    }
    let mut request = match builder.body(Body::from(body)) {
        Ok(request) => request,
        Err(_) => return write_error(&mut write, 400, "Bad Request", "bad request syntax").await,
    };
    if negative_length {
        request
            .extensions_mut()
            .insert(crate::http_adapter::PythonNegativeLength);
    }
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));
    let response = match app.oneshot(request).await {
        Ok(response) => response,
        Err(_) => {
            return write_json_error(&mut write, 500, "internal error: HTTP adapter failed").await
        }
    };
    write_response(&mut write, response).await
}

fn python_content_length(value: &str) -> Option<(bool, bool, bool)> {
    // HTTPMessage decodes header octets as ISO-8859-1 before Python calls int;
    // a UTF-8 Arabic digit therefore arrives as two non-decimal Latin-1 chars.
    let value = value.trim();
    let (negative, digits) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    let bytes = digits.as_bytes();
    if bytes.is_empty()
        || bytes[0] == b'_'
        || *bytes.last()? == b'_'
        || bytes.windows(2).any(|pair| pair == b"__")
        || bytes
            .iter()
            .any(|byte| !byte.is_ascii_digit() && *byte != b'_')
    {
        return None;
    }
    let normalized = digits.replace('_', "");
    let magnitude = normalized.trim_start_matches('0');
    let zero = magnitude.is_empty();
    let too_large = magnitude.len() > 4 || (magnitude.len() == 4 && magnitude > "4096");
    Some((negative && !zero, zero, too_large))
}

fn python_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(value.len() + 2);
    out.push(quote);
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            value if value == quote => {
                out.push('\\');
                out.push(value);
            }
            value => out.push(value),
        }
    }
    out.push(quote);
    out
}

async fn read_line_bounded<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    line: &mut Vec<u8>,
    limit: usize,
) -> io::Result<bool> {
    line.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(!line.is_empty());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |index| index + 1);
        if line.len() + count > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP line too long",
            ));
        }
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() {
            return Ok(true);
        }
    }
}

fn trim_crlf(line: &[u8]) -> String {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    line.iter().map(|byte| char::from(*byte)).collect()
}

async fn write_response(
    write: &mut tokio::net::tcp::OwnedWriteHalf,
    response: Response<Body>,
) -> io::Result<()> {
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(io::Error::other)?;
    let reason = python_reason(status);
    write
        .write_all(format!("HTTP/1.0 {} {reason}\r\n", status.as_u16()).as_bytes())
        .await?;
    write
        .write_all(b"Content-Type: application/json\r\n")
        .await?;
    write
        .write_all(format!("Content-Length: {}\r\n", body.len()).as_bytes())
        .await?;
    write.write_all(b"Cache-Control: no-store\r\n\r\n").await?;
    write.write_all(&body).await?;
    write.shutdown().await
}

fn python_reason(status: StatusCode) -> &'static str {
    match status.as_u16() {
        413 => "Request Entity Too Large",
        _ => status.canonical_reason().unwrap_or("Unknown"),
    }
}

async fn write_json_error(
    write: &mut tokio::net::tcp::OwnedWriteHalf,
    status: u16,
    message: &str,
) -> io::Result<()> {
    let payload = crate::python_json::Value::Object(vec![(
        crate::python_json::PythonString::from_str("error"),
        crate::python_json::Value::String(crate::python_json::PythonString::from_str(message)),
    )]);
    let response = crate::http_adapter::EncodedResponse::json(status, &payload, false)
        .into_axum()
        .map_err(io::Error::other)?;
    write_response(write, response).await
}

async fn write_error(
    write: &mut tokio::net::tcp::OwnedWriteHalf,
    status: u16,
    reason: &str,
    message: &str,
) -> io::Result<()> {
    let explanation = match status {
        400 => "Bad request syntax or unsupported method",
        414 => "URI is too long",
        431 => {
            "The server is unwilling to process the request because its header fields are too large"
        }
        501 => "Server does not support this operation",
        _ => reason,
    };
    let body = format!(
        "<!DOCTYPE HTML>\n<html lang=\"en\">\n    <head>\n        <meta charset=\"utf-8\">\n        <style type=\"text/css\">\n            :root {{\n                color-scheme: light dark;\n            }}\n        </style>\n        <title>Error response</title>\n    </head>\n    <body>\n        <h1>Error response</h1>\n        <p>Error code: {status}</p>\n        <p>Message: {}.</p>\n        <p>Error code explanation: {status} - {explanation}.</p>\n    </body>\n</html>\n",
        html_escape(message),
    );
    write
        .write_all(
            format!(
                "HTTP/1.0 {status} {reason}\r\nContent-Type: text/html;charset=utf-8\r\nContent-Length: {}\r\n\r\n{body}",
                body.len(),
            )
            .as_bytes(),
        )
        .await?;
    write.shutdown().await
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, Read, Write},
        net::TcpStream,
        process::{Command, Stdio},
        thread,
        time::Duration,
    };

    fn raw_request(app: Router, bytes: &[u8]) -> Vec<u8> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            runtime.block_on(async move {
                let (stream, peer) = listener.accept().await.unwrap();
                serve_connection(stream, peer, app).await.unwrap();
            })
        });
        let mut client = TcpStream::connect(address).unwrap();
        client.write_all(bytes).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        server.join().unwrap();
        response
    }

    #[test]
    fn raw_duplicate_lengths_short_eof_and_chunked_without_length_reach_router() {
        use crate::http_adapter::{router, tests::FakeBackend, tests::FakeDaemon};
        for request in [
            b"POST /no-route HTTP/1.0\r\nContent-Length: 4\r\nContent-Length: 100\r\n\r\n{}"
                .as_slice(),
            b"POST /no-route HTTP/1.0\r\nContent-Length: 100\r\nContent-Length: 4\r\n\r\n{}"
                .as_slice(),
            b"POST /no-route HTTP/1.0\r\nContent-Length: 100\r\n\r\n{}".as_slice(),
            b"POST /no-route HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".as_slice(),
        ] {
            let response = raw_request(
                router(FakeDaemon::default(), FakeBackend::default()),
                request,
            );
            assert!(
                response.starts_with(b"HTTP/1.0 404 Not Found\r\n"),
                "{response:?}"
            );
            assert!(response.ends_with(b"{\n  \"error\": \"no write route /no-route\"\n}"));
        }
    }

    #[test]
    fn raw_tcp_matches_actual_python_handler_on_framing_and_fake_resize_errors() {
        use crate::http_adapter::{router, tests::FakeDaemon};
        use ds_core::http::Backend;
        use serde_json::{json, Map, Value as SerdeValue};
        use std::collections::BTreeSet;

        #[derive(Default)]
        struct OracleBackend;
        impl Backend for OracleBackend {
            fn name(&self) -> String {
                "fake".into()
            }
            fn capabilities(&self) -> BTreeSet<String> {
                ["resize".into()].into()
            }
            fn list_panes_grouped(&self) -> Vec<SerdeValue> {
                vec![json!({"pane_id":"w1:p1","label":"test"})]
            }
            fn resize(
                &self,
                _pane_id: &str,
                cols: &SerdeValue,
                rows: &SerdeValue,
            ) -> Result<Map<String, SerdeValue>, String> {
                for value in [cols, rows] {
                    let integer = match value {
                        SerdeValue::Bool(value) => {
                            if *value {
                                1
                            } else {
                                0
                            }
                        }
                        SerdeValue::Number(number) => number
                            .as_i64()
                            .or_else(|| number.as_u64().map(|value| value as i64))
                            .or_else(|| number.as_f64().map(|value| value.trunc() as i64))
                            .ok_or_else(|| "cols and rows must be integers".to_owned())?,
                        SerdeValue::String(value) => crate::http_adapter::python_int_string(value)
                            .ok_or_else(|| "cols and rows must be integers".to_owned())?,
                        _ => return Err("cols and rows must be integers".into()),
                    };
                    if !(1..=1000).contains(&integer) {
                        return Err("cols and rows must be between 1 and 1000".into());
                    }
                }
                Err("fake refusal".into())
            }
        }
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let script = r#"
import dunk_http, dunk_terminal
from http.server import ThreadingHTTPServer
class FakeBackend:
    name = 'fake'
    def capabilities(self): return {'resize'}
    def list_panes_grouped(self): return [{'pane_id':'w1:p1','label':'test'}]
    def resize(self, pane_id, cols, rows):
        try: cols, rows = int(cols), int(rows)
        except (TypeError, ValueError): return False, 'cols and rows must be integers'
        if not (1 <= cols <= 1000 and 1 <= rows <= 1000): return False, 'cols and rows must be between 1 and 1000'
        return False, 'fake refusal'
dunk_terminal.get_backend = lambda: FakeBackend()
dunk_http._now = lambda: '2020-01-01T00:00:00Z'
server = ThreadingHTTPServer(('127.0.0.1', 0), dunk_http.Handler)
print(server.server_port, flush=True)
server.serve_forever()
"#;
        let mut child = Command::new("python3")
            .args(["-u", "-c", script])
            .env("PYTHONPATH", root)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start CPython oracle");
        let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut port = String::new();
        output.read_line(&mut port).unwrap();
        let port = port.trim().parse::<u16>().expect("oracle port");

        let requests: [&[u8]; 19] = [
            b"POST /no-route HTTP/1.0\r\nContent-Length: 4\r\nContent-Length: 100\r\n\r\n{\"x\":1}",
            b"POST /no-route HTTP/1.0\r\nContent-Length: 100\r\nContent-Length: 4\r\n\r\n{\"x\":1}",
            b"POST /no-route HTTP/1.0\r\nContent-Length: 100\r\n\r\n{}",
            b"POST /no-route HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            b"POST /no-route HTTP/1.0\r\nContent-Length: 4097\r\n\r\n",
            b"POST /no-route HTTP/1.0\r\nContent-Length: 1\r\n\r\n{",
            b"POST /no-route HTTP/1.0\r\nContent-Length: nope\r\n\r\n",
            b"POST /api/terminals/test/resize HTTP/1.0\r\nContent-Length: 100\r\n\r\n{\"cols\":\"\\ud800\",\"rows\":24}",
            b"POST /api/terminals/test/resize HTTP/1.0\r\nContent-Length: 100\r\n\r\n{\"cols\":999999999999999999999999999999999999999999999999,\"rows\":24}",
            b"POST /api/terminals/test/resize HTTP/1.0\r\nContent-Length: 100\r\n\r\n{\"cols\":[1,2],\"rows\":24}",
            b"POST /api/terminals/test/resize HTTP/1.0\r\nContent-Length: 100\r\n\r\n{\"cols\":{\"nested\":1},\"rows\":24}",
            b"POST /api/terminals/test/resize HTTP/1.0\r\nContent-Length: 100\r\n\r\n{\"cols\":80,\"rows\":24}",
            b"PUT /healthz HTTP/1.0\r\nContent-Length: 0\r\n\r\n",
            b"POST /api/terminals/test/resize HTTP/1.0\r\nContent-Length: 100\r\n\r\n{\"cols\":NaN,\"rows\":24}",
            b"POST /api/terminals/test/resize HTTP/1.0\r\nContent-Length: 100\r\n\r\n{\"cols\":Infinity,\"rows\":24}",
            b"POST /api/terminals/test/resize HTTP/1.0\r\nContent-Length: 100\r\n\r\n{\"cols\":-Infinity,\"rows\":24}",
            b"POST /api/terminals/test/resize HTTP/1.0\r\nContent-Length: 100\r\n\r\n{\"cols\":1e999,\"rows\":24}",
            b"POST /api/terminals/test/resize HTTP/1.0\r\nContent-Length: 100\r\n\r\n{\"cols\":\"1_0\",\"rows\":24}",
            b"POST /api/terminals/test/resize HTTP/1.0\r\nContent-Length: 100\r\n\r\n{\"cols\":\"\\u0661\",\"rows\":24}",
        ];
        for (request_bytes, index) in requests.into_iter().zip(0..) {
            compare_oracle_request(
                port,
                router(FakeDaemon::default(), OracleBackend),
                request_bytes,
                index,
            );
        }
        let mut request = b"POST /no-route HTTP/1.0\r\nContent-Length: 4096\r\n\r\n".to_vec();
        request.extend_from_slice(b"{}");
        request.resize(request.len() + 4094, b' ');
        compare_oracle_request(
            port,
            router(FakeDaemon::default(), OracleBackend),
            &request,
            19,
        );

        let mut long_request_line = b"GET /".to_vec();
        long_request_line.resize(MAX_LINE + 1, b'a');
        long_request_line.extend_from_slice(b"\r\n");
        compare_oracle_request(
            port,
            router(FakeDaemon::default(), OracleBackend),
            &long_request_line,
            20,
        );
        let mut long_header_line = b"GET /healthz HTTP/1.0\r\nX: ".to_vec();
        long_header_line.resize(MAX_LINE + 1, b'a');
        long_header_line.extend_from_slice(b"\r\n\r\n");
        compare_oracle_request(
            port,
            router(FakeDaemon::default(), OracleBackend),
            &long_header_line,
            21,
        );
        let mut too_many_headers = b"GET /healthz HTTP/1.0\r\n".to_vec();
        for _ in 0..101 {
            too_many_headers.extend_from_slice(b"X: y\r\n");
        }
        too_many_headers.extend_from_slice(b"\r\n");
        compare_oracle_request(
            port,
            router(FakeDaemon::default(), OracleBackend),
            &too_many_headers,
            22,
        );
        compare_oracle_request(
            port,
            router(FakeDaemon::default(), OracleBackend),
            b"GARBAGE\r\n",
            23,
        );
        compare_oracle_request(
            port,
            router(FakeDaemon::default(), OracleBackend),
            b"POST /no-route HTTP/1.0\r\nContent-Length: \xd9\xa1\r\n\r\n",
            24,
        );
        compare_expect_continue(port, router(FakeDaemon::default(), OracleBackend));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    fn compare_oracle_request(port: u16, app: Router, request: &[u8], case: usize) {
        let rust = raw_request(app, request);
        let mut python = TcpStream::connect(("127.0.0.1", port)).unwrap();
        python.write_all(request).unwrap();
        python.shutdown(std::net::Shutdown::Write).unwrap();
        let mut python_response = Vec::new();
        python.read_to_end(&mut python_response).unwrap();
        let compare_error_page = !matches!(case, 12 | 20 | 21 | 22 | 23);
        assert_response_equivalent(&python_response, &rust, case, compare_error_page);
    }

    fn compare_expect_continue(port: u16, app: Router) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            runtime.block_on(async move {
                let (stream, peer) = listener.accept().await.unwrap();
                serve_connection(stream, peer, app).await.unwrap();
            })
        });
        let headers = b"POST /no-route HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\nContent-Length: 2\r\n\r\n";
        let mut rust = TcpStream::connect(address).unwrap();
        rust.write_all(headers).unwrap();
        rust.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut byte = [0_u8; 1];
        assert!(
            matches!(
                rust.read(&mut byte),
                Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
            ),
            "Rust listener sent an unexpected interim response"
        );
        rust.set_read_timeout(None).unwrap();
        rust.write_all(b"{}").unwrap();
        rust.shutdown(std::net::Shutdown::Write).unwrap();
        let mut rust_response = Vec::new();
        rust.read_to_end(&mut rust_response).unwrap();
        server.join().unwrap();

        let mut python = TcpStream::connect(("127.0.0.1", port)).unwrap();
        python.write_all(headers).unwrap();
        python
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        assert!(
            matches!(
                python.read(&mut byte),
                Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
            ),
            "Python Handler sent an unexpected interim response"
        );
        python.set_read_timeout(None).unwrap();
        python.write_all(b"{}").unwrap();
        python.shutdown(std::net::Shutdown::Write).unwrap();
        let mut python_response = Vec::new();
        python.read_to_end(&mut python_response).unwrap();
        assert_response_equivalent(&python_response, &rust_response, 25, true);
    }

    fn assert_response_equivalent(python: &[u8], rust: &[u8], case: usize, compare_body: bool) {
        fn split(response: &[u8]) -> (u16, Vec<(String, String)>, &[u8]) {
            let boundary = response
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .unwrap();
            let head = std::str::from_utf8(&response[..boundary]).unwrap();
            let mut lines = head.split("\r\n");
            let status = lines
                .next()
                .unwrap()
                .split_ascii_whitespace()
                .nth(1)
                .unwrap()
                .parse()
                .unwrap();
            let headers = lines
                .filter_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    Some((name.to_ascii_lowercase(), value.trim().to_owned()))
                })
                .collect();
            (status, headers, &response[boundary + 4..])
        }
        let (py_status, py_headers, py_body) = split(python);
        let (rs_status, rs_headers, rs_body) = split(rust);
        assert_eq!(
            py_status, rs_status,
            "status case {case}: {python:?} vs {rust:?}"
        );
        if compare_body {
            assert_eq!(py_body, rs_body, "body case {case}: {python:?} vs {rust:?}");
        } else {
            assert_eq!(
                py_headers
                    .iter()
                    .find(|(key, _)| key == "content-length")
                    .unwrap()
                    .1
                    .parse::<usize>()
                    .unwrap(),
                py_body.len()
            );
            assert_eq!(
                rs_headers
                    .iter()
                    .find(|(key, _)| key == "content-length")
                    .unwrap()
                    .1
                    .parse::<usize>()
                    .unwrap(),
                rs_body.len()
            );
        }
        for name in if compare_body {
            &["content-type", "content-length", "cache-control"][..]
        } else {
            &["content-type", "cache-control"][..]
        } {
            assert_eq!(
                py_headers.iter().find(|(key, _)| key == name),
                rs_headers.iter().find(|(key, _)| key == name),
                "header {name} case {case}"
            );
        }
    }
}
