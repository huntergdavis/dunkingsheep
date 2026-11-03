//! Request-scoped inbox identity and backend sources.

use super::RequestRuntimeAdapter;
use crate::error::DunkError;

fn mailbox_override_from(value: Option<&str>) -> Option<String> {
    value
        .map(|s| s.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

impl RequestRuntimeAdapter<'_> {
    #[allow(dead_code)] // The complete InboxEnv assembly belongs to its dependent leaf.
    fn helper_inboxenv_caller_role(&self, pane_id: &str) -> Result<String, DunkError> {
        self.terminal
            .flock
            .caller_role(Some(pane_id), &self.terminal)
    }

    #[allow(dead_code)] // The complete InboxEnv assembly belongs to its dependent leaf.
    fn helper_inboxenv_mailbox_override(&self) -> Option<String> {
        mailbox_override_from(std::env::var("DUNKINGSHEEP_MAILBOX").ok().as_deref())
    }

    #[allow(dead_code)] // The complete InboxEnv assembly belongs to its dependent leaf.
    fn helper_inboxenv_backend_name(&self) -> Result<String, DunkError> {
        super::configured_backend_name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Env,
        flock::Flock,
        roles::Roles,
        server::RequestCtx,
        terminal::{fakes::FakeTerminal, Terminal},
    };
    use std::sync::Mutex;

    #[test]
    fn production_env_inbox_data_uses_concrete_services() {
        let dir = crate::testsupport::dir("inbox-data-runtime");
        let flock = Flock::new(dir.path().join("launches"), dir.path().join("lanes"), false);
        let request = RequestCtx::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
        assert!(adapter.helper_inboxenv_caller_role("w1:p1").is_err());
        let mut roles = Roles::new(
            &Env::default(),
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .unwrap();
        roles
            .register("w1:p1", "reviewer", None, None, None, None, None, None)
            .unwrap();
        flock.attach_roles(roles, None);
        assert_eq!(
            adapter.helper_inboxenv_caller_role("w1:p1").unwrap(),
            "reviewer"
        );
        assert_eq!(
            adapter.helper_inboxenv_backend_name().unwrap(),
            super::super::configured_backend_name().unwrap()
        );
        assert_eq!(
            adapter.helper_inboxenv_mailbox_override(),
            mailbox_override_from(std::env::var("DUNKINGSHEEP_MAILBOX").ok().as_deref())
        );
        assert_eq!(
            mailbox_override_from(Some(" \u{1c}lane\u{1f} ")),
            Some("lane".into())
        );
        assert_eq!(mailbox_override_from(Some(" \u{1c}\t")), None);
        assert_eq!(mailbox_override_from(None), None);
    }
}
