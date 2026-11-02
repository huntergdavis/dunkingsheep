//! Runtime backend configuration shared by the server and startup adapters.
//!
//! The Python selector trims and lowercases `DUNKINGSHEEP_TERMINAL_BACKEND`,
//! defaulting to tmux. Herdr is retired in the Rust runtime, so it is refused
//! explicitly instead of being silently replaced with tmux.

use std::ffi::OsStr;

use crate::error::{refusal, DunkError};
use crate::terminal::tmux::TmuxBackend;

pub const TERMINAL_BACKEND_ENV: &str = "DUNKINGSHEEP_TERMINAL_BACKEND";
pub const DEFAULT_TERMINAL_BACKEND: &str = "tmux";

/// Return the normalized configured backend name, using tmux when unset or blank.
/// A configured value that cannot be represented as UTF-8 is a refusal, never a default.
pub fn configured_backend_name() -> Result<String, DunkError> {
    configured_backend_name_from(std::env::var_os(TERMINAL_BACKEND_ENV).as_deref())
}

/// Construct the configured backend, refusing retired and unknown names explicitly.
pub fn configured_backend() -> Result<TmuxBackend, DunkError> {
    let name = configured_backend_name()?;
    backend_from_name(Some(&name))
}

fn configured_backend_name_from(configured: Option<&OsStr>) -> Result<String, DunkError> {
    let Some(configured) = configured else {
        return Ok(DEFAULT_TERMINAL_BACKEND.to_owned());
    };
    let Some(configured) = configured.to_str() else {
        return Err(refusal(
            "terminal backend setting is not valid UTF-8; expected 'tmux'",
        ));
    };
    Ok(normalize_backend_name(Some(configured)))
}

fn normalize_backend_name(configured: Option<&str>) -> String {
    let name = configured.unwrap_or_default().trim().to_lowercase();
    if name.is_empty() {
        DEFAULT_TERMINAL_BACKEND.to_owned()
    } else {
        name
    }
}

fn backend_from_name(configured: Option<&str>) -> Result<TmuxBackend, DunkError> {
    let name = normalize_backend_name(configured);
    match name.as_str() {
        "tmux" => Ok(TmuxBackend::default()),
        "herdr" => Err(refusal(
            "terminal backend 'herdr' is retired; expected 'tmux'",
        )),
        _ => Err(refusal(format!(
            "unknown terminal backend {name:?}; expected 'tmux'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_backend_selects_tmux_by_default_and_refuses_retired_or_unknown_names() {
        let default = backend_from_name(None).expect("unset backend selects tmux");
        assert_eq!(default.tmux_bin, std::path::PathBuf::from("tmux"));
        assert!(backend_from_name(Some("   ")).is_ok());
        assert!(backend_from_name(Some(" TMUX ")).is_ok());
        assert_eq!(
            backend_from_name(Some(" HerDr ")).unwrap_err().to_string(),
            "terminal backend 'herdr' is retired; expected 'tmux'"
        );
        assert_eq!(
            backend_from_name(Some("other")).unwrap_err().to_string(),
            "unknown terminal backend \"other\"; expected 'tmux'"
        );
    }

    #[cfg(unix)]
    #[test]
    fn configured_backend_refuses_non_utf8_environment_names() {
        use std::os::unix::ffi::OsStrExt;

        let invalid_value = OsStr::from_bytes(b"tmux\xff");
        let error = configured_backend_name_from(Some(invalid_value)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "terminal backend setting is not valid UTF-8; expected 'tmux'"
        );
    }
}
