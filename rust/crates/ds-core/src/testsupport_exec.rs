/// Retry only ETXTBSY while a child may retain the fixture's writer descriptor.
/// The existing bound is 400 retries separated by five milliseconds.
pub fn exec_retrying_busy<T>(mut exec: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut attempts = 0;
    loop {
        match exec() {
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 400 =>
            {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            other => return other,
        }
    }
}

/// Settle a newly written executable without requiring its ordinary exit code to be zero.
/// Shell fixtures made by `write_fake_shell` exit before their normal body on this call.
pub fn warm_up_executable(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    let path = path.as_ref();
    exec_retrying_busy(|| {
        std::process::Command::new(path)
            .arg("--help")
            .env("DS_TEST_EXEC_WARMUP", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
    })?;
    Ok(())
}

/// Write and settle a shell mock. Its private warm-up skips counters, logs, stdin and sleeps;
/// ordinary invocations receive the original body and argument list.
pub fn write_fake_shell(
    path: impl AsRef<std::path::Path>,
    script: impl AsRef<[u8]>,
    mode: u32,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = path.as_ref();
    let script = script.as_ref();
    let newline = script.iter().position(|&b| b == b'\n').ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "shell fixture has no shebang line",
        )
    })?;
    let header = &script[..newline];
    if ![
        b"#!/bin/sh".as_slice(),
        b"#!/bin/bash",
        b"#!/usr/bin/env sh",
        b"#!/usr/bin/env bash",
    ]
    .contains(&header)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "shell fixture requires a supported sh/bash shebang",
        ));
    }
    let mut guarded = Vec::with_capacity(script.len() + 80);
    guarded.extend_from_slice(&script[..=newline]);
    guarded.extend_from_slice(b"if [ \"${DS_TEST_EXEC_WARMUP:-}\" = 1 ]; then exit 0; fi\n");
    guarded.extend_from_slice(&script[newline + 1..]);
    std::fs::write(path, guarded)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    warm_up_executable(path)
}
