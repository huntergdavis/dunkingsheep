//! The daemon's production GitHub App token provider (t3241, audit P0-3).
//!
//! `RuntimeServices` builds exactly one accepted [`Tokens`] cache when it is constructed. The
//! cache resolves everything from the same captured [`Env`] that places the daemon's other
//! stores: the config directory is `config::config_dir` (`DUNKINGSHEEP_DIR`, else
//! `HOME/.config/dunkingsheep`), the cache is `github-tokens.json` in that directory (mode
//! 0600), and the App registrations and API root are the paths Python reads. Requests borrow
//! the cache through `RequestRuntimeAdapter::github_tokens`; nothing rebuilds it per request,
//! so the in-memory cache, its operation lock and the persisted file share one lifecycle. The
//! App calls keep the accepted reqwest transport.
//!
//! Construction never fails the daemon. When the config directory cannot be resolved, the
//! reason is stored and reported by the first command that needs a token, so a daemon that
//! never touches GitHub starts exactly as it did before.

use std::path::PathBuf;

use crate::config::{self, Env};
use crate::error::{refusal, DunkError};
use crate::github::{OsAppFileSystem, ReqwestTransport, Tokens, DEFAULT_API};

/// One process's GitHub token provider, owned by `RuntimeServices`.
pub(crate) struct GitHubProvider {
    tokens: Result<Tokens, String>,
}

impl GitHubProvider {
    /// The production composition for the captured environment.
    pub(crate) fn production(env: &Env) -> Self {
        let api_root = nonblank(env.get("DUNKINGSHEEP_GITHUB_API"))
            .unwrap_or(DEFAULT_API)
            .to_owned();
        let tokens = config::config_dir(env)
            .map_err(|error| error.to_string())
            .map(|config_dir| {
                let path = nonblank(env.get("DUNKINGSHEEP_GITHUB_TOKENS_FILE"))
                    .map(PathBuf::from)
                    .unwrap_or_else(|| config_dir.join("github-tokens.json"));
                Tokens::with_app_file_system(
                    Some(path),
                    config_dir,
                    Box::new(ReqwestTransport::default()),
                    Some(api_root),
                    Box::new(OsAppFileSystem),
                )
            });
        Self { tokens }
    }

    /// Explicit composition for tests: a fake App endpoint and fixture App files.
    #[cfg(test)]
    pub(crate) fn with_parts(
        path: PathBuf,
        config_dir: PathBuf,
        transport: Box<dyn crate::github::GitHubTransport>,
        api_root: &str,
        app_files: Box<dyn crate::github::AppFileSystem>,
    ) -> Self {
        Self {
            tokens: Ok(Tokens::with_app_file_system(
                Some(path),
                config_dir,
                transport,
                Some(api_root.to_owned()),
                app_files,
            )),
        }
    }

    /// The canonical cache, or the refusal that stopped it from being built.
    pub(crate) fn tokens(&self) -> Result<&Tokens, DunkError> {
        self.tokens
            .as_ref()
            .map_err(|message| refusal(message.clone()))
    }
}

/// A blank override counts as unset, as Python's `environ.get(...) or default` does.
fn nonblank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[cfg(test)]
#[path = "github_provider_tests.rs"]
mod tests;
