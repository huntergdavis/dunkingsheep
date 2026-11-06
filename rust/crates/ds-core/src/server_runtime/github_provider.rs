//! The daemon's production GitHub token provider (t3241, audit P0-3).
//!
//! `RuntimeServices` builds exactly one accepted [`Tokens`] cache and one review transport
//! when it is constructed, and both come from the same captured [`Env`] that places the
//! daemon's other stores. The config directory is `config::config_dir` (`DUNKINGSHEEP_DIR`,
//! else `HOME/.config/dunkingsheep`). The cache is `github-tokens.json` in that directory
//! (mode 0600). The App registrations and API root are read from the same place Python reads
//! them. Requests borrow the cache through `RequestRuntimeAdapter::github_tokens`, and the
//! review service borrows the transport through `RequestRuntimeAdapter::review_transport`.
//! Nothing is rebuilt per request, so the in-memory cache, its operation lock and the
//! persisted file share one lifecycle.
//!
//! Construction performs no network I/O and never fails the daemon. The two reqwest clients
//! are built here, once. When the config directory cannot be resolved, the reason is stored
//! and reported by the first command that needs a token.

use std::path::PathBuf;

use crate::config::{self, Env};
use crate::error::{refusal, DunkError};
use crate::github::{GitHubTransport, OsAppFileSystem, ReqwestTransport, Tokens, DEFAULT_API};

/// One process's GitHub token provider, owned by `RuntimeServices`.
pub(crate) struct GitHubProvider {
    tokens: Result<Tokens, String>,
    /// The transport the review service calls GitHub through.
    review: Box<dyn GitHubTransport>,
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
        Self {
            tokens,
            review: Box::new(ReqwestTransport::default()),
        }
    }

    /// Explicit composition for tests: a fake App endpoint, a fake review transport, and
    /// fixture App files.
    #[cfg(test)]
    pub(crate) fn with_parts(
        path: PathBuf,
        config_dir: PathBuf,
        transport: Box<dyn GitHubTransport>,
        review: Box<dyn GitHubTransport>,
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
            review,
        }
    }

    /// The canonical cache, or the refusal that stopped it from being built.
    pub(crate) fn tokens(&self) -> Result<&Tokens, DunkError> {
        self.tokens
            .as_ref()
            .map_err(|message| refusal(message.clone()))
    }

    /// The transport the review service calls GitHub through.
    pub(crate) fn review_transport(&self) -> &dyn GitHubTransport {
        self.review.as_ref()
    }
}

/// A blank or whitespace-only override counts as unset. `config::config_dir` already treats a
/// blank `DUNKINGSHEEP_DIR` this way. Python's `environ.get(key, default)` keeps an explicitly
/// empty value instead, so this is a deliberate difference for blank overrides.
fn nonblank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[cfg(test)]
#[path = "github_provider_tests.rs"]
mod tests;
