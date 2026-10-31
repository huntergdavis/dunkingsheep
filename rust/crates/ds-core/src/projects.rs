//! `project_spawn` (t1171, contract in docs/PROJECTS.md): the front desk asks, the DAEMON
//! starts a project with its own authority. Slice 1: the guards, the idempotent and
//! resumable step runner, and the events - driven through the `Terminal` trait so a test
//! uses `FakeTerminal` and never a real agent.
//!
//! NOT in this slice (named so nobody reads it as done): wiring into the command table and
//! `ALLOW['desk']`, persistence of the registry, the owner-mailbox report, model choice by
//! usage headroom, and the "one word" approval transport (docs/PROJECTS.md "Not decided").
//! The live registry is in memory, so a daemon restart forgets a half-spawned project.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::DunkError;
use crate::terminal::{AgentSpec, Terminal};

/// Max agents one project may own (named in the refusal text). docs/PROJECTS.md leaves the
/// exact maxima undecided; these are the starting values, not a measured limit.
pub const MAX_AGENTS_PER_PROJECT: usize = 4;
/// Max projects live at once.
pub const MAX_LIVE_PROJECTS: usize = 5;
/// Five-hour Claude usage at or above this refuses a spawn (the Rust-gate headroom number).
pub const USAGE_REFUSE_PCT: f64 = 70.0;

/// A real usage reading. `Unknown` is never headroom (the fleet's standing rule).
#[derive(Debug, Clone, PartialEq)]
pub enum Usage {
    Percent(f64),
    Unknown(String),
}

/// The steps of a spawn, in order; each is an event, so a half-spawned project is visible.
pub const STEPS: [&str; 5] = [
    "validated",
    "scaffolded",
    "workspace",
    "research_agent",
    "reported",
];

/// One logged step.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub step: String,
    pub ok: bool,
    pub detail: String,
}

/// Where a project is.
#[derive(Debug, Clone, PartialEq)]
pub enum State {
    /// Waiting for the owner's one word.
    AwaitingApproval,
    /// Steps remain (a step failed; calling `spawn` again resumes).
    Partial,
    Live,
}

/// One project in the registry.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub name: String,
    pub dir: PathBuf,
    pub brief: String,
    pub soft_cap: Option<f64>,
    pub state: State,
    pub workspace_id: Option<String>,
    /// (role, pane id).
    pub agents: Vec<(String, String)>,
    pub events: Vec<Event>,
}

impl Project {
    fn done(&self, step: &str) -> bool {
        self.events.iter().any(|e| e.step == step && e.ok)
    }
}

/// What the desk sends.
#[derive(Debug, Clone)]
pub struct Request {
    pub name: String,
    pub brief: String,
    pub template: String,
    pub soft_cap: Option<f64>,
    /// The owner's one word has been given.
    pub approved: bool,
    /// The command that starts the research agent (the caller picked the model).
    pub research_argv: Vec<String>,
}

/// The registry of projects this daemon started.
#[derive(Debug, Default)]
pub struct Projects {
    projects: Vec<Project>,
}

/// `[a-z0-9][a-z0-9-]{1,38}`: a slug, because the name becomes a directory under the root.
pub fn validate_name(name: &str) -> Result<String, DunkError> {
    let name = name.trim().to_string();
    let ok = (2..=39).contains(&name.len())
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.ends_with('-');
    if ok {
        Ok(name)
    } else {
        Err(DunkError::Refused(format!(
            "project name {name:?} must be a slug: 2-39 characters of a-z, 0-9 and '-', not starting with '-' or ending in one"
        )))
    }
}

/// The usage gate: refuse at or above `USAGE_REFUSE_PCT`, and refuse an unknown reading.
pub fn usage_gate(usage: &Usage) -> Result<(), DunkError> {
    match usage {
        Usage::Percent(pct) if *pct >= USAGE_REFUSE_PCT => Err(DunkError::Refused(format!(
            "five-hour usage is {pct:.0}% (>= {USAGE_REFUSE_PCT:.0}%): no new project until it drops"
        ))),
        Usage::Percent(_) => Ok(()),
        Usage::Unknown(why) => Err(DunkError::Refused(format!(
            "five-hour usage is unknown ({why}); an unknown reading is never headroom"
        ))),
    }
}

impl Projects {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, name: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.name == name)
    }

    fn live_count(&self) -> usize {
        self.projects
            .iter()
            .filter(|p| p.state == State::Live)
            .count()
    }

    /// `project_spawn`: validate, guard, approve, then run the steps that are not done.
    /// Idempotent by name: a live project is returned untouched; a partial one resumes.
    pub fn spawn(
        &mut self,
        terminal: &mut dyn Terminal,
        root: &Path,
        request: &Request,
        usage: &Usage,
    ) -> Result<Project, DunkError> {
        let name = validate_name(&request.name)?;
        if request.template != "standard" {
            return Err(DunkError::Refused(format!(
                "unknown project template {:?}; only 'standard' exists",
                request.template
            )));
        }
        if let Some(existing) = self.get(&name) {
            if existing.state == State::Live {
                return Ok(existing.clone());
            }
        } else {
            if self.live_count() >= MAX_LIVE_PROJECTS {
                return Err(DunkError::Refused(format!(
                    "{MAX_LIVE_PROJECTS} projects are already live (MAX_LIVE_PROJECTS); stop one first"
                )));
            }
            usage_gate(usage)?;
            self.projects.push(Project {
                name: name.clone(),
                dir: root.join(&name),
                brief: request.brief.clone(),
                soft_cap: request.soft_cap,
                state: State::AwaitingApproval,
                workspace_id: None,
                agents: Vec::new(),
                events: Vec::new(),
            });
        }
        let index = self
            .projects
            .iter()
            .position(|p| p.name == name)
            .unwrap_or(0);
        if !request.approved {
            self.projects[index].state = State::AwaitingApproval;
            return Ok(self.projects[index].clone());
        }
        // A resume re-checks usage: headroom can vanish between the two calls.
        usage_gate(usage)?;
        let project = &mut self.projects[index];
        project.state = State::Partial;
        let result = run_steps(project, terminal, request);
        if result.is_ok() {
            project.state = State::Live;
        }
        result.map(|()| project.clone())
    }
}

fn log(project: &mut Project, step: &str, ok: bool, detail: impl Into<String>) {
    project.events.push(Event {
        step: step.to_string(),
        ok,
        detail: detail.into(),
    });
}

fn fail(project: &mut Project, step: &str, message: String) -> Result<(), DunkError> {
    log(project, step, false, message.clone());
    Err(DunkError::Refused(format!(
        "project {}: step {step} failed: {message}",
        project.name
    )))
}

fn run_steps(
    project: &mut Project,
    terminal: &mut dyn Terminal,
    request: &Request,
) -> Result<(), DunkError> {
    if !project.done("validated") {
        log(
            project,
            "validated",
            true,
            format!("template {}", request.template),
        );
    }
    if !project.done("scaffolded") {
        match scaffold(&project.dir, &project.brief) {
            Ok(()) => log(
                project,
                "scaffolded",
                true,
                project.dir.display().to_string(),
            ),
            Err(message) => return fail(project, "scaffolded", message),
        }
    }
    if !project.done("workspace") {
        let cwd = project.dir.display().to_string();
        match terminal.create_workspace(Some(&project.name), Some(&cwd), false) {
            Ok(workspace) => {
                project.workspace_id = Some(workspace.workspace_id.clone());
                log(project, "workspace", true, workspace.workspace_id);
            }
            Err(error) => return fail(project, "workspace", error.to_string()),
        }
    }
    if !project.done("research_agent") {
        if project.agents.len() >= MAX_AGENTS_PER_PROJECT {
            return fail(
                project,
                "research_agent",
                format!("{MAX_AGENTS_PER_PROJECT} agents already (MAX_AGENTS_PER_PROJECT)"),
            );
        }
        if request.research_argv.is_empty() {
            return fail(
                project,
                "research_agent",
                "no launch command for the research agent".into(),
            );
        }
        let spec = AgentSpec {
            name: format!("{}-research", project.name),
            argv: request.research_argv.clone(),
            workspace_id: project.workspace_id.clone(),
            cwd: Some(project.dir.display().to_string()),
            label: Some(format!("{}-research", project.name)),
            ..AgentSpec::default()
        };
        match terminal.start_agent(&spec) {
            Ok(pane) => {
                project
                    .agents
                    .push(("research".into(), pane.pane_id.clone()));
                log(project, "research_agent", true, pane.pane_id);
            }
            Err(error) => return fail(project, "research_agent", error.to_string()),
        }
    }
    if !project.done("reported") {
        // The owner-mailbox report needs the inbox store (a later slice); the event records
        // what the report will carry so the step order is already the contract's.
        let panes: Vec<String> = project
            .agents
            .iter()
            .map(|(r, p)| format!("{r}={p}"))
            .collect();
        log(project, "reported", true, panes.join(","));
    }
    Ok(())
}

/// Step 3: `~/workspace/<name>`, `git init`, `BRIEF.md`, a `dunkingsheep/` dir.
fn scaffold(dir: &Path, brief: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir.join("dunkingsheep"))
        .map_err(|e| format!("create {}: {e}", dir.display()))?;
    std::fs::write(dir.join("BRIEF.md"), format!("{}\n", brief.trim_end()))
        .map_err(|e| format!("write BRIEF.md: {e}"))?;
    if !dir.join(".git").exists() {
        let out = Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(dir)
            .output()
            .map_err(|e| format!("git init: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git init: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::fakes::FakeTerminal;

    fn request(name: &str, approved: bool) -> Request {
        Request {
            name: name.into(),
            brief: "build a thing".into(),
            template: "standard".into(),
            soft_cap: None,
            approved,
            research_argv: vec!["claude".into()],
        }
    }

    fn root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ds-projects-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn names_are_slugs() {
        assert_eq!(validate_name(" my-app ").unwrap(), "my-app");
        for bad in ["", "a", "-x", "x-", "Up", "a b", "../etc", &"a".repeat(40)] {
            assert!(validate_name(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn usage_gate_refuses_at_the_ceiling_and_when_unknown() {
        assert!(usage_gate(&Usage::Percent(69.9)).is_ok());
        assert!(usage_gate(&Usage::Percent(70.0))
            .unwrap_err()
            .to_string()
            .contains("70%"));
        assert!(usage_gate(&Usage::Unknown("probe timed out".into()))
            .unwrap_err()
            .to_string()
            .contains("never headroom"));
    }

    #[test]
    fn nothing_starts_until_the_owner_says_the_word() {
        let (mut term, mut projects, root) =
            (FakeTerminal::new(), Projects::new(), root("approve"));
        let waiting = projects
            .spawn(
                &mut term,
                &root,
                &request("app", false),
                &Usage::Percent(10.0),
            )
            .unwrap();
        assert_eq!(State::AwaitingApproval, waiting.state);
        assert!(
            term.layout_calls.is_empty() && !root.join("app").exists(),
            "no side effect before approval"
        );
        let live = projects
            .spawn(
                &mut term,
                &root,
                &request("app", true),
                &Usage::Percent(10.0),
            )
            .unwrap();
        assert_eq!(State::Live, live.state);
        assert!(
            root.join("app/BRIEF.md").exists()
                && root.join("app/dunkingsheep").is_dir()
                && root.join("app/.git").exists()
        );
        assert_eq!(1, live.agents.len());
        let steps: Vec<&str> = live.events.iter().map(|e| e.step.as_str()).collect();
        assert_eq!(steps, STEPS.to_vec());
    }

    #[test]
    fn spawn_is_idempotent_by_name() {
        let (mut term, mut projects, root) = (FakeTerminal::new(), Projects::new(), root("idem"));
        let first = projects
            .spawn(
                &mut term,
                &root,
                &request("app", true),
                &Usage::Percent(10.0),
            )
            .unwrap();
        let calls = term.layout_calls.len();
        let again = projects
            .spawn(
                &mut term,
                &root,
                &request("app", true),
                &Usage::Percent(99.0),
            )
            .unwrap();
        assert_eq!(
            first, again,
            "a live project is returned untouched, even when usage is now high"
        );
        assert_eq!(
            calls,
            term.layout_calls.len(),
            "and nothing is started twice"
        );
    }

    #[test]
    fn a_failed_step_is_logged_and_the_next_call_resumes() {
        let (mut term, mut projects, root) = (FakeTerminal::new(), Projects::new(), root("resume"));
        term.fail_layout = Some("no server".into());
        let err = projects
            .spawn(
                &mut term,
                &root,
                &request("app", true),
                &Usage::Percent(10.0),
            )
            .unwrap_err();
        assert!(err.to_string().contains("step workspace failed"), "{err}");
        let partial = projects.get("app").unwrap();
        assert_eq!(State::Partial, partial.state);
        assert!(partial.done("scaffolded") && !partial.done("workspace"));
        let live = projects
            .spawn(
                &mut term,
                &root,
                &request("app", true),
                &Usage::Percent(10.0),
            )
            .unwrap();
        assert_eq!(State::Live, live.state);
        assert_eq!(
            1,
            live.events
                .iter()
                .filter(|e| e.step == "scaffolded")
                .count(),
            "scaffold ran once"
        );
    }

    #[test]
    fn the_guards_name_themselves() {
        let (mut term, mut projects, root) = (FakeTerminal::new(), Projects::new(), root("guards"));
        for index in 0..MAX_LIVE_PROJECTS {
            projects
                .spawn(
                    &mut term,
                    &root,
                    &request(&format!("p{index}x"), true),
                    &Usage::Percent(1.0),
                )
                .unwrap();
        }
        let err = projects
            .spawn(
                &mut term,
                &root,
                &request("extra", true),
                &Usage::Percent(1.0),
            )
            .unwrap_err();
        assert!(err.to_string().contains("MAX_LIVE_PROJECTS"), "{err}");
        // A separate registry, so the refusal below is the usage gate and not the live cap.
        let hot = Projects::new().spawn(
            &mut FakeTerminal::new(),
            &root,
            &request("zz", true),
            &Usage::Percent(90.0),
        );
        assert!(hot
            .unwrap_err()
            .to_string()
            .contains("five-hour usage is 90%"));
        let template = Request {
            template: "huge".into(),
            ..request("tt", true)
        };
        assert!(projects
            .spawn(&mut term, &root, &template, &Usage::Percent(1.0))
            .unwrap_err()
            .to_string()
            .contains("only 'standard'"));
    }
}
