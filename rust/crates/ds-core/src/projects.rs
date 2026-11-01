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
use crate::fallback::{tier_headroom, tier_state, Credit, RESERVE_PERCENT};
use crate::terminal::{AgentSpec, Terminal};

/// Max agents one project may own (named in the refusal text). docs/PROJECTS.md leaves the
/// exact maxima undecided; these are the starting values, not a measured limit.
pub const MAX_AGENTS_PER_PROJECT: usize = 4;
/// Max projects live at once.
pub const MAX_LIVE_PROJECTS: usize = 5;
/// Max names waiting for the owner at once: an unapproved name is a registry entry too, and
/// an unbounded desk could fill the registry with them.
pub const MAX_AWAITING_APPROVAL: usize = 5;
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

/// What the desk sends: the contract's `project_spawn(name, brief, template, soft_cap)` and
/// nothing else. Whether the owner approved, and which command starts the research agent,
/// are NOT the caller's to say - see [`Authority`].
#[derive(Debug, Clone)]
pub struct Request {
    pub name: String,
    pub brief: String,
    pub template: String,
    pub soft_cap: Option<f64>,
}

/// What the DAEMON knows and the desk must never supply: the owner's one word, and the
/// command that starts the research agent (the daemon picks the model). A desk-controlled
/// `research_argv` would be an arbitrary command run with daemon authority.
#[derive(Debug, Clone)]
pub struct Authority {
    /// The owner's one word has been given (read from the owner's mailbox, daemon-side).
    pub approved: bool,
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

/// The role of a project agent, for choosing what to launch it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Research,
    Worker,
    Reviewer,
}

/// Tier order for workers and reviewers: the cheap lane first. The owner's order (docs/PROJECTS.md):
/// the newest deepseek flash, then codex when that is out of budget, then claude. The `pi` tier
/// reads the `opencode-go` rows, which is where deepseek runs (`fallback::tier_providers`).
/// Reviewers follow the worker order: the owner's words name only workers and research agents.
pub const WORKER_TIER_ORDER: [&str; 3] = ["pi", "codex", "claude"];

/// Tier order for research agents: the reverse (claude, then codex, then the cheap lane).
pub const RESEARCH_TIER_ORDER: [&str; 3] = ["claude", "codex", "pi"];

/// Choose the tier to launch a project agent on, by REAL usage headroom: the first tier in the
/// role's order that is positively available AND has finite headroom strictly above the reserve
/// (`fallback::RESERVE_PERCENT`, the same number the model-fallback planner uses). Unknown credit
/// is not credit and a missing headroom number is not headroom, so a tier is never chosen on
/// absence of evidence. No tier qualifies -> a refusal naming every tier's state, so the owner
/// can see why nothing was started.
pub fn pick_tier(role: Role, credit: &Credit) -> Result<&'static str, DunkError> {
    let order: &[&str] = if role == Role::Research {
        &RESEARCH_TIER_ORDER
    } else {
        &WORKER_TIER_ORDER
    };
    let mut why = Vec::new();
    for tier in order {
        let state = tier_state(credit, tier);
        match (state, tier_headroom(credit, tier)) {
            ("available", Some(left)) if left > RESERVE_PERCENT => return Ok(tier),
            ("available", Some(left)) => why.push(format!(
                "{tier} available ({left:?}% left, not above the {RESERVE_PERCENT} reserve)"
            )),
            ("available", None) => why.push(format!("{tier} available (headroom unknown)")),
            (other, Some(left)) => why.push(format!("{tier} {other} ({left:?}% left)")),
            (other, None) => why.push(format!("{tier} {other}")),
        }
    }
    let who = match role {
        Role::Research => "a research agent",
        Role::Worker => "a worker",
        Role::Reviewer => "a reviewer",
    };
    Err(DunkError::Refused(format!(
        "no tier has usable headroom for {who}: {}",
        why.join("; ")
    )))
}

impl Projects {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, name: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.name == name)
    }

    fn awaiting_count(&self) -> usize {
        self.projects
            .iter()
            .filter(|p| p.state == State::AwaitingApproval)
            .count()
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
        authority: &Authority,
        usage: &Usage,
    ) -> Result<Project, DunkError> {
        let name = validate_name(&request.name)?;
        if let Some(cap) = request.soft_cap {
            if !cap.is_finite() || cap <= 0.0 {
                return Err(DunkError::Refused(format!(
                    "soft_cap must be a positive number; got {cap}"
                )));
            }
        }
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
            if self.awaiting_count() >= MAX_AWAITING_APPROVAL {
                return Err(DunkError::Refused(format!(
                    "{MAX_AWAITING_APPROVAL} projects already wait for the owner (MAX_AWAITING_APPROVAL); wait for an answer first"
                )));
            }
            // The registry is in memory, so it cannot tell a directory that is ours from one
            // that is not: a name already on disk (a shared checkout such as 'dunkingsheep',
            // or a project from before a restart) is refused, never overwritten.
            let dir = root.join(&name);
            if dir.exists() {
                return Err(DunkError::Refused(format!(
                    "{} already exists and is not a project this daemon started; pick another name",
                    dir.display()
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
        if !authority.approved {
            self.projects[index].state = State::AwaitingApproval;
            return Ok(self.projects[index].clone());
        }
        // A resume re-checks usage: headroom can vanish between the two calls.
        usage_gate(usage)?;
        let project = &mut self.projects[index];
        project.state = State::Partial;
        let result = run_steps(project, terminal, request, authority);
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
    authority: &Authority,
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
        if authority.research_argv.is_empty() {
            return fail(
                project,
                "research_agent",
                "no launch command for the research agent".into(),
            );
        }
        let spec = AgentSpec {
            name: format!("{}-research", project.name),
            argv: authority.research_argv.clone(),
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
        // NOTHING IS SENT: the owner-mailbox report needs the inbox store (a later slice). The
        // step is logged as NOT ok, with what the report will carry, so a Live project never
        // claims a report it did not make.
        let panes: Vec<String> = project
            .agents
            .iter()
            .map(|(r, p)| format!("{r}={p}"))
            .collect();
        log(
            project,
            "reported",
            false,
            format!(
                "pending: not sent (owner mailbox not wired); would carry {}",
                panes.join(",")
            ),
        );
    }
    Ok(())
}

/// Step 3: `~/workspace/<name>`, `git init`, `BRIEF.md`, a `dunkingsheep/` dir.
fn scaffold(dir: &Path, brief: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir.join("dunkingsheep"))
        .map_err(|e| format!("create {}: {e}", dir.display()))?;
    // Only if absent: a resumed scaffold, or a hand-edited brief, is never overwritten.
    let brief_path = dir.join("BRIEF.md");
    if !brief_path.exists() {
        std::fs::write(&brief_path, format!("{}\n", brief.trim_end()))
            .map_err(|e| format!("write BRIEF.md: {e}"))?;
    }
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

    fn request(name: &str) -> Request {
        Request {
            name: name.into(),
            brief: "build a thing".into(),
            template: "standard".into(),
            soft_cap: None,
        }
    }

    fn authority(approved: bool) -> Authority {
        Authority {
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
                &request("app"),
                &authority(false),
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
                &request("app"),
                &authority(true),
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
                &request("app"),
                &authority(true),
                &Usage::Percent(10.0),
            )
            .unwrap();
        let calls = term.layout_calls.len();
        let again = projects
            .spawn(
                &mut term,
                &root,
                &request("app"),
                &authority(true),
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
                &request("app"),
                &authority(true),
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
                &request("app"),
                &authority(true),
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
                    &request(&format!("p{index}x")),
                    &authority(true),
                    &Usage::Percent(1.0),
                )
                .unwrap();
        }
        let err = projects
            .spawn(
                &mut term,
                &root,
                &request("extra"),
                &authority(true),
                &Usage::Percent(1.0),
            )
            .unwrap_err();
        assert!(err.to_string().contains("MAX_LIVE_PROJECTS"), "{err}");
        // A separate registry, so the refusal below is the usage gate and not the live cap.
        let hot = Projects::new().spawn(
            &mut FakeTerminal::new(),
            &root,
            &request("zz"),
            &authority(true),
            &Usage::Percent(90.0),
        );
        assert!(hot
            .unwrap_err()
            .to_string()
            .contains("five-hour usage is 90%"));
        let template = Request {
            template: "huge".into(),
            ..request("tt")
        };
        assert!(projects
            .spawn(
                &mut term,
                &root,
                &template,
                &authority(true),
                &Usage::Percent(1.0)
            )
            .unwrap_err()
            .to_string()
            .contains("only 'standard'"));
    }

    #[test]
    fn a_name_already_on_disk_is_refused_and_its_files_are_untouched() {
        // Finding 1: BRIEF.md of an existing directory (a shared checkout, or a project from
        // before a restart) was overwritten and a research agent started in it.
        let (mut term, mut projects, root) = (FakeTerminal::new(), Projects::new(), root("exists"));
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(root.join("app/BRIEF.md"), "PRECIOUS").unwrap();
        let refused = projects
            .spawn(
                &mut term,
                &root,
                &request("app"),
                &authority(true),
                &Usage::Percent(10.0),
            )
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("already exists and is not a project this daemon started"),
            "{refused}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("app/BRIEF.md")).unwrap(),
            "PRECIOUS"
        );
        assert!(term.layout_calls.is_empty(), "no workspace, no agent");
        assert!(projects.get("app").is_none(), "and no registry entry");
    }

    #[test]
    fn a_resumed_scaffold_never_overwrites_the_brief() {
        let (mut term, mut projects, root) =
            (FakeTerminal::new(), Projects::new(), root("keepbrief"));
        term.fail_layout = Some("no server".into());
        let _ = projects.spawn(
            &mut term,
            &root,
            &request("app"),
            &authority(true),
            &Usage::Percent(10.0),
        );
        std::fs::write(root.join("app/BRIEF.md"), "edited by hand").unwrap();
        term.fail_layout = None;
        projects
            .spawn(
                &mut term,
                &root,
                &request("app"),
                &authority(true),
                &Usage::Percent(10.0),
            )
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("app/BRIEF.md")).unwrap(),
            "edited by hand"
        );
    }

    #[test]
    fn the_desk_cannot_supply_approval_or_the_launch_command() {
        // Finding 2: approval and the research command are the DAEMON's (Authority); the
        // desk's Request has neither field, so the types enforce what a review cannot.
        let (mut term, mut projects, root) =
            (FakeTerminal::new(), Projects::new(), root("authority"));
        let nobody = Authority {
            approved: true,
            research_argv: vec![],
        };
        let err = projects
            .spawn(
                &mut term,
                &root,
                &request("app"),
                &nobody,
                &Usage::Percent(10.0),
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no launch command for the research agent"),
            "{err}"
        );
    }

    #[test]
    fn the_report_step_says_it_was_not_sent() {
        // Finding 3: a Live project must not claim a report nothing made.
        let (mut term, mut projects, root) =
            (FakeTerminal::new(), Projects::new(), root("reported"));
        let live = projects
            .spawn(
                &mut term,
                &root,
                &request("app"),
                &authority(true),
                &Usage::Percent(10.0),
            )
            .unwrap();
        let reported = live.events.iter().find(|e| e.step == "reported").unwrap();
        assert!(!reported.ok);
        assert!(
            reported.detail.contains("pending: not sent"),
            "{}",
            reported.detail
        );
    }

    #[test]
    fn unapproved_names_are_bounded_and_a_bad_soft_cap_is_refused() {
        // Finding 4.
        let (mut term, mut projects, root) =
            (FakeTerminal::new(), Projects::new(), root("bounded"));
        for index in 0..MAX_AWAITING_APPROVAL {
            projects
                .spawn(
                    &mut term,
                    &root,
                    &request(&format!("w{index}x")),
                    &authority(false),
                    &Usage::Percent(1.0),
                )
                .unwrap();
        }
        let refused = projects
            .spawn(
                &mut term,
                &root,
                &request("onemore"),
                &authority(false),
                &Usage::Percent(1.0),
            )
            .unwrap_err()
            .to_string();
        assert!(refused.contains("MAX_AWAITING_APPROVAL"), "{refused}");
        for bad in [-1.0, 0.0, f64::NAN, f64::INFINITY] {
            let capped = Request {
                soft_cap: Some(bad),
                ..request("capped")
            };
            assert!(
                projects
                    .spawn(
                        &mut term,
                        &root,
                        &capped,
                        &authority(true),
                        &Usage::Percent(1.0)
                    )
                    .is_err(),
                "{bad}"
            );
        }
        // A sane cap is accepted (on a fresh registry: this one is full of waiting names).
        let fine = Request {
            soft_cap: Some(25.0),
            ..request("fine")
        };
        let waiting = Projects::new()
            .spawn(
                &mut term,
                &root,
                &fine,
                &authority(false),
                &Usage::Percent(1.0),
            )
            .unwrap();
        assert_eq!(waiting.soft_cap, Some(25.0));
    }

    fn credit(states: &[(&str, &str)], headroom: &[(&str, f64)]) -> Credit {
        let mut c = Credit::default();
        for (k, v) in states {
            c.states.insert((*k).into(), (*v).into());
        }
        for (k, v) in headroom {
            c.headroom.insert((*k).into(), *v);
        }
        c
    }

    fn all_available() -> Credit {
        credit(
            &[
                ("claude", "available"),
                ("codex", "available"),
                ("opencode-go", "available"),
            ],
            &[("claude", 90.0), ("codex", 80.0), ("opencode-go", 70.0)],
        )
    }

    #[test]
    fn workers_take_the_cheap_lane_first_and_research_takes_claude_first() {
        let c = all_available();
        assert_eq!(pick_tier(Role::Worker, &c).unwrap(), "pi");
        assert_eq!(
            pick_tier(Role::Reviewer, &c).unwrap(),
            "pi",
            "reviewers follow the worker order"
        );
        assert_eq!(pick_tier(Role::Research, &c).unwrap(), "claude");
    }

    #[test]
    fn an_exhausted_or_unknown_tier_is_skipped_in_each_orders_direction() {
        // deepseek (opencode-go) spent: workers move to codex, then claude
        let mut c = all_available();
        c.states.insert("opencode-go".into(), "exhausted".into());
        assert_eq!(pick_tier(Role::Worker, &c).unwrap(), "codex");
        c.states.insert("codex".into(), "unknown".into());
        assert_eq!(pick_tier(Role::Worker, &c).unwrap(), "claude");
        // claude spent: research moves to codex, then the cheap lane
        let mut r = all_available();
        r.states.insert("claude".into(), "exhausted".into());
        assert_eq!(pick_tier(Role::Research, &r).unwrap(), "codex");
        r.states.insert("codex".into(), "exhausted".into());
        assert_eq!(pick_tier(Role::Research, &r).unwrap(), "pi");
    }

    #[test]
    fn the_reserve_is_strict_and_a_missing_number_is_not_headroom() {
        let at_five = credit(
            &[("opencode-go", "available"), ("codex", "available")],
            &[("opencode-go", 5.0), ("codex", 5.1)],
        );
        assert_eq!(
            pick_tier(Role::Worker, &at_five).unwrap(),
            "codex",
            "exactly 5% is refused, 5.1% is allowed"
        );
        let no_numbers = credit(&[("opencode-go", "available")], &[]);
        let err = pick_tier(Role::Worker, &no_numbers)
            .unwrap_err()
            .to_string();
        assert!(err.contains("pi available (headroom unknown)"), "{err}");
        let nan = credit(
            &[("opencode-go", "available")],
            &[("opencode-go", f64::NAN)],
        );
        assert!(
            pick_tier(Role::Worker, &nan).is_err(),
            "a NaN is not headroom"
        );
    }

    #[test]
    fn nothing_usable_is_a_refusal_that_names_every_tier() {
        let spent = credit(
            &[
                ("claude", "exhausted"),
                ("codex", "available"),
                ("opencode-go", "unknown"),
            ],
            &[("claude", 0.0), ("codex", 3.0)],
        );
        let err = pick_tier(Role::Worker, &spent).unwrap_err().to_string();
        assert_eq!(
            err,
            "no tier has usable headroom for a worker: pi unknown; codex available (3.0% left, not above the 5 reserve); claude exhausted (0.0% left)"
        );
        let research = pick_tier(Role::Research, &spent).unwrap_err().to_string();
        assert!(
            research
                .starts_with("no tier has usable headroom for a research agent: claude exhausted"),
            "{research}"
        );
        assert!(
            pick_tier(Role::Worker, &Credit::default()).is_err(),
            "an empty snapshot picks nothing"
        );
    }

    #[test]
    fn a_free_tier_is_never_a_project_target() {
        // Even a funded free row is outside both orders: a project agent is not placed on a
        // resource nobody measured for this purpose.
        let c = credit(&[("free", "available")], &[("free", 100.0)]);
        assert!(pick_tier(Role::Worker, &c).is_err());
        assert!(!WORKER_TIER_ORDER.contains(&"free") && !RESEARCH_TIER_ORDER.contains(&"free"));
    }
}
