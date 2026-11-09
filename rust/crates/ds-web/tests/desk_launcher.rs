//! Exercise the production binary with private state and an absent daemon.
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn production_launcher_serves_private_desk_and_stops_on_term() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("ui/dist")).unwrap();
    fs::write(root.join("ui/dist/index.html"), "desk fixture").unwrap();
    fs::write(
        root.join("channels.json"),
        r#"[{"id":"cos","name":"Chief","match":"cos","kind":"active"}]"#,
    )
    .unwrap();
    fs::write(root.join("token"), "fixture-token\n").unwrap();
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let mut child = Command::new(env!("CARGO_BIN_EXE_ds-desk"))
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("DUNKINGSHEEP_DIR", root.join("config"))
        .env("VP_DATA_DIR", root.join("data"))
        .env("DUNKINGSHEEP_SERVICE_TOKEN_FILE", root.join("token"))
        .env("VP_PASSWORD", "fixture-password")
        .env("VP_BIND", "127.0.0.1")
        .env("VP_PORT", port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(20);
    let mut stream = loop {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
            break s;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "desk exited before listening"
        );
        assert!(Instant::now() < until, "desk did not start");
        thread::sleep(Duration::from_millis(50));
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"GET /api/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(root.join("data/auth.json").is_file());
    assert!(Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .unwrap()
        .success());
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            panic!("desk did not shut down");
        }
        thread::sleep(Duration::from_millis(50));
    }
}
