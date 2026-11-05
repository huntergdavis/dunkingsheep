//! `project_spawn` (t1171): the front desk asks, the DAEMON starts a project with its own
//! authority. Project state, durable step events, agent starts, and owner reporting are
//! kept behind this module so a restart can resume without replaying an uncertain action.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::DunkError;
use crate::fallback::{tier_headroom, tier_state, Credit, RESERVE_PERCENT};
use crate::terminal::{AgentSpec, Pane, Terminal, TerminalError, Workspace};
use serde::{Deserialize, Serialize};

pub mod newprojects;

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

/// The project runtime only needs these two mutations. Unlike `Terminal`, this narrow seam lets
/// the request adapter acquire the shared terminal lock for one actual operation at a time.
pub trait ProjectTerminal {
    fn create_workspace(
        &mut self,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Workspace, TerminalError>;

    fn start_agent(&mut self, spec: &AgentSpec) -> Result<Pane, TerminalError>;
}

impl<T: Terminal + ?Sized> ProjectTerminal for T {
    fn create_workspace(
        &mut self,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Workspace, TerminalError> {
        Terminal::create_workspace(self, label, cwd, focus)
    }

    fn start_agent(&mut self, spec: &AgentSpec) -> Result<Pane, TerminalError> {
        Terminal::start_agent(self, spec)
    }
}

/// Runtime role registration is a separate durable operation from starting a pane. If it fails,
/// the caller retains the pane in project state and refuses an automatic duplicate start.
pub trait ProjectAgentRegistrar {
    fn register(
        &self,
        role: &str,
        pane: &Pane,
        provider: &str,
        model: &str,
        label: &str,
    ) -> Result<(), String>;
}

/// The steps of a spawn, in order; each is an event, so a half-spawned project is visible.
pub const STEPS: [&str; 6] = [
    "validated",
    "scaffolded",
    "workspace",
    "writer_agent",
    "reviewer_agent",
    "reported",
];

/// One logged step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub step: String,
    pub ok: bool,
    pub detail: String,
}

/// Where a project is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum State {
    /// Waiting for the owner's one word.
    AwaitingApproval,
    /// Steps remain (a step failed; calling `spawn` again resumes).
    Partial,
    Live,
    /// Stopped by the owner (`project_stop`): its agents and workspace are closed, its directory
    /// is kept. A stopped name is never silently revived by `spawn`.
    Stopped,
}

/// One project in the registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// A durable intent written before an external action. On restart, an outstanding intent
    /// is reported as uncertain and is never replayed automatically.
    #[serde(default)]
    pub pending_step: Option<String>,
}

impl Project {
    fn done(&self, step: &str) -> bool {
        self.events.iter().any(|e| e.step == step && e.ok)
    }
}

/// What the desk sends: the contract's `project_spawn(name, brief, template, soft_cap)` and
/// nothing else. Whether the owner approved, and the commands used for project agents,
/// are NOT the caller's to say - see [`Authority`].
#[derive(Debug, Clone)]
pub struct Request {
    pub name: String,
    pub brief: String,
    pub template: String,
    pub soft_cap: Option<f64>,
}

/// What the DAEMON knows and the desk must never supply: the owner's one word. Project agent
/// commands are built from the approved template below; a desk-controlled argv would be an
/// arbitrary command run with daemon authority.
#[derive(Debug, Clone)]
pub struct Authority {
    /// The owner's one word has been given (read from the owner's mailbox, daemon-side).
    pub approved: bool,
    /// Writer and reviewer argv are explicit daemon authority, never request fields.
    pub writer_argv: Vec<String>,
    pub reviewer_argv: Vec<String>,
}

impl Authority {
    /// The approved project template: one Luna low writer and one Luna medium reviewer.
    /// Budget admission is performed against the configured Codex subscription before this
    /// template can cause any project write or agent start.
    pub fn project_template(approved: bool) -> Self {
        let argv = |effort: &str| {
            vec![
                "codex".into(),
                "--no-daemon".into(),
                "--model".into(),
                "gpt-6-luna".into(),
                "--config".into(),
                format!("model_reasoning_effort={effort}"),
            ]
        };
        Self {
            approved,
            writer_argv: argv("low"),
            reviewer_argv: argv("medium"),
        }
    }
}

/// Posts the "project is live" report to the owner's mailbox (docs/PROJECTS.md step 7). A trait
/// so the registry is tested without a mailbox; the daemon's implementation sends through the
/// inbox store. `Ok` carries the message id the report was filed under.
pub trait Reporter: std::fmt::Debug {
    fn report(&self, project: &str, body: &str) -> Result<String, String>;
}

/// The registry of projects this daemon started.
#[derive(Debug, Default)]
pub struct Projects {
    projects: Vec<Project>,
    /// None until the owner mailbox is wired: the `reported` step then says it was NOT sent.
    reporter: Option<Box<dyn Reporter>>,
    store: Option<newprojects::ProjectStore>,
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

    /// Open the project-owned durable store and recover its registry before accepting requests.
    /// The path must come from approved configuration; no amount or store path is invented.
    pub async fn open_durable(path: &Path) -> Result<Self, DunkError> {
        let store = newprojects::ProjectStore::open(path).await?;
        let projects = store.load().await?;
        Ok(Self {
            projects,
            reporter: None,
            store: Some(store),
        })
    }

    pub async fn open_durable_with_reporter(
        path: &Path,
        reporter: Box<dyn Reporter>,
    ) -> Result<Self, DunkError> {
        let store = newprojects::ProjectStore::open(path).await?;
        let projects = store.load().await?;
        Ok(Self {
            projects,
            reporter: Some(reporter),
            store: Some(store),
        })
    }

    pub async fn shutdown(&self) {
        if let Some(store) = &self.store {
            store.shutdown().await;
        }
    }

    /// Budgeted async entry point. The legacy synchronous entry point has no budget configuration
    /// and is therefore refused before it can write a project.
    #[allow(clippy::too_many_arguments)]
    async fn spawn_budgeted<T: ProjectTerminal + ?Sized>(
        &mut self,
        terminal: &mut T,
        root: &Path,
        request: &Request,
        authority: &Authority,
        five_hour_usage: &Usage,
        budget: &newprojects::ProjectBudgetConfig,
        provider: &str,
        provider_reading: &serde_json::Value,
        now: f64,
        reporter_override: Option<&dyn Reporter>,
        registrar: Option<&dyn ProjectAgentRegistrar>,
    ) -> Result<Project, DunkError> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| DunkError::Refused("project budget not configured".into()))?;
        let name = validate_name(&request.name)?;
        if self
            .get(&name)
            .is_some_and(|project| project.pending_step.is_some())
        {
            return Err(DunkError::Refused(format!(
                "project {name}: previous step outcome is uncertain; inspect before retry"
            )));
        }
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
            if existing.state == State::Stopped {
                return Err(DunkError::Refused(format!(
                    "project {name:?} was stopped by the owner (its directory is kept); start a new project under another name"
                )));
            }
            if existing.state == State::Live {
                return Ok(existing.clone());
            }
            if authority.approved && reporter_override.is_none() && self.reporter.is_none() {
                return Err(DunkError::Refused(
                    "owner mailbox reporter is not wired".into(),
                ));
            }
            let approved_root = canonical_project_root(root)?;
            validate_owned_project_dir(&approved_root, &name, existing)?;
            if !existing.done("scaffolded") && project_path_present(&existing.dir)? {
                return Err(DunkError::Refused(format!(
                    "{} already exists without a durable scaffold completion; inspect it before retry",
                    existing.dir.display()
                )));
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
            let approved_root = canonical_project_root(root)?;
            let dir = approved_root.join(&name);
            if project_path_present(&dir)? {
                return Err(DunkError::Refused(format!(
                    "{} already exists and is not a project this daemon started; pick another name",
                    dir.display()
                )));
            }
            if authority.approved && reporter_override.is_none() && self.reporter.is_none() {
                return Err(DunkError::Refused(
                    "owner mailbox reporter is not wired".into(),
                ));
            }
            // A new name is budget-admitted before any durable project state is written.
            budget.admit(&name, provider, five_hour_usage, provider_reading, now)?;
            let project = Project {
                name: name.clone(),
                dir,
                brief: request.brief.clone(),
                soft_cap: request.soft_cap,
                state: State::AwaitingApproval,
                workspace_id: None,
                agents: Vec::new(),
                events: Vec::new(),
                pending_step: None,
            };
            store.save(project.clone(), None).await?;
            self.projects.push(project);
        }

        let index = self
            .projects
            .iter()
            .position(|project| project.name == name)
            .ok_or_else(|| {
                DunkError::Refused(format!("project {name}: durable registry lost its state"))
            })?;
        if !authority.approved {
            self.projects[index].state = State::AwaitingApproval;
            store.save(self.projects[index].clone(), None).await?;
            return Ok(self.projects[index].clone());
        }
        // Revalidate current provider evidence before any side effect on a resumed project.
        budget.admit(&name, provider, five_hour_usage, provider_reading, now)?;
        self.projects[index].state = State::Partial;
        store.save(self.projects[index].clone(), None).await?;
        let reporter = reporter_override.or(self.reporter.as_deref());
        run_steps_durable(
            &mut self.projects[index],
            terminal,
            request,
            authority,
            reporter,
            registrar,
            store,
        )
        .await?;
        self.projects[index].state = State::Live;
        store.save(self.projects[index].clone(), None).await?;
        Ok(self.projects[index].clone())
    }

    /// Production-facing provider path: run the configured real usage reader, then validate that
    /// measurement and persist only after admission succeeds.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_from_sources<T: Terminal + ?Sized>(
        &mut self,
        terminal: &mut T,
        root: &Path,
        request: &Request,
        authority: &Authority,
        five_hour_usage: &Usage,
        budget_json: &str,
        provider: &str,
        provider_home: &Path,
        now: f64,
    ) -> Result<Project, DunkError> {
        self.spawn_from_sources_reporter(
            terminal,
            root,
            request,
            authority,
            five_hour_usage,
            budget_json,
            provider,
            provider_home,
            now,
            None,
        )
        .await
    }

    /// Production-facing variant with a request-scoped owner mailbox reporter. The reporter is
    /// borrowed only for this call, so a daemon can use its existing inbox adapter without
    /// storing borrowed runtime state inside `Projects`.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_from_sources_with_reporter<T: Terminal + ?Sized>(
        &mut self,
        terminal: &mut T,
        root: &Path,
        request: &Request,
        authority: &Authority,
        five_hour_usage: &Usage,
        budget_json: &str,
        provider: &str,
        provider_home: &Path,
        now: f64,
        reporter: &dyn Reporter,
    ) -> Result<Project, DunkError> {
        self.spawn_from_sources_reporter(
            terminal,
            root,
            request,
            authority,
            five_hour_usage,
            budget_json,
            provider,
            provider_home,
            now,
            Some(reporter),
        )
        .await
    }

    /// Production entry point when the request adapter has already captured and validated a
    /// provider reading before opening the durable store. Role registration is awaited as part
    /// of each agent step, after the pane identity has been persisted.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_from_reading_with_reporter_and_registrar<T: ProjectTerminal + ?Sized>(
        &mut self,
        terminal: &mut T,
        root: &Path,
        request: &Request,
        authority: &Authority,
        five_hour_usage: &Usage,
        budget_json: &str,
        provider: &str,
        provider_reading: &serde_json::Value,
        now: f64,
        reporter: &dyn Reporter,
        registrar: &dyn ProjectAgentRegistrar,
    ) -> Result<Project, DunkError> {
        let budget = newprojects::ProjectBudgetConfig::from_json(budget_json)?;
        self.spawn_budgeted(
            terminal,
            root,
            request,
            authority,
            five_hour_usage,
            &budget,
            provider,
            provider_reading,
            now,
            Some(reporter),
            Some(registrar),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_from_sources_reporter<T: Terminal + ?Sized>(
        &mut self,
        terminal: &mut T,
        root: &Path,
        request: &Request,
        authority: &Authority,
        five_hour_usage: &Usage,
        budget_json: &str,
        provider: &str,
        provider_home: &Path,
        now: f64,
        reporter: Option<&dyn Reporter>,
    ) -> Result<Project, DunkError> {
        let budget = newprojects::ProjectBudgetConfig::from_json(budget_json)?;
        let reading = budget.read_provider(provider, provider_home, now).await?;
        self.spawn_budgeted(
            terminal,
            root,
            request,
            authority,
            five_hour_usage,
            &budget,
            provider,
            &reading,
            now,
            reporter,
            None,
        )
        .await
    }

    /// A registry that posts its report through `reporter`.
    pub fn with_reporter(reporter: Box<dyn Reporter>) -> Self {
        Projects {
            reporter: Some(reporter),
            ..Self::default()
        }
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

    /// `project_stop`: close a project's agents and workspace. OWNER-ONLY (docs/PROJECTS.md): the
    /// desk can start a project but closing and stopping the fleet stays with a human; the caller
    /// (the daemon's role check) supplies `is_owner`, the desk never can. Every close is an event.
    /// An agent whose pane is closed is dropped from the record, so a retry after a failure closes
    /// only what is left. The project DIRECTORY (BRIEF.md, the repo, the owner's work) is kept.
    /// Idempotent: stopping a stopped project closes nothing more.
    pub fn stop(
        &mut self,
        terminal: &mut dyn Terminal,
        name: &str,
        is_owner: bool,
    ) -> Result<Project, DunkError> {
        if !is_owner {
            return Err(DunkError::Refused(
                "project_stop is owner-only: closing and stopping the fleet stays with a human in a terminal".into(),
            ));
        }
        if self.store.is_some() {
            return Err(DunkError::Refused(
                "durable projects require stop_durable so state and events are awaited".into(),
            ));
        }
        let Some(index) = self.projects.iter().position(|p| p.name == name) else {
            return Err(DunkError::Refused(format!("no project named {name:?}")));
        };
        let project = &mut self.projects[index];
        if project.state == State::Stopped {
            return Ok(project.clone());
        }
        let mut failures = Vec::new();
        for (role, pane_id) in project.agents.clone() {
            match terminal.close("pane", &pane_id) {
                Ok(()) => {
                    project.agents.retain(|(_, p)| *p != pane_id);
                    log(
                        project,
                        "stop_agent",
                        true,
                        format!("closed {role}={pane_id}"),
                    );
                }
                Err(error) => {
                    log(
                        project,
                        "stop_agent",
                        false,
                        format!("{role}={pane_id}: {error}"),
                    );
                    failures.push(format!("{role}={pane_id}: {error}"));
                }
            }
        }
        if failures.is_empty() {
            if let Some(workspace) = project.workspace_id.clone() {
                match terminal.close("workspace", &workspace) {
                    Ok(()) => {
                        project.workspace_id = None;
                        log(
                            project,
                            "stop_workspace",
                            true,
                            format!("closed {workspace}"),
                        );
                    }
                    Err(error) => {
                        log(
                            project,
                            "stop_workspace",
                            false,
                            format!("{workspace}: {error}"),
                        );
                        failures.push(format!("workspace {workspace}: {error}"));
                    }
                }
            }
        }
        if !failures.is_empty() {
            return Err(DunkError::Refused(format!(
                "project {name}: stop incomplete ({}); calling project_stop again retries what is left",
                failures.join("; ")
            )));
        }
        project.state = State::Stopped;
        let kept = project.dir.display().to_string();
        log(
            project,
            "stopped",
            true,
            format!("agents and workspace closed; directory kept at {kept}"),
        );
        Ok(project.clone())
    }

    /// Durable counterpart to `stop`: persist each close intent before the terminal call and its
    /// result before proceeding. A recovered unresolved intent is never replayed automatically.
    pub async fn stop_durable<T: Terminal + Send>(
        &mut self,
        terminal: &mut T,
        name: &str,
        is_owner: bool,
    ) -> Result<Project, DunkError> {
        if !is_owner {
            return Err(DunkError::Refused(
                "project_stop is owner-only: closing and stopping the fleet stays with a human in a terminal".into(),
            ));
        }
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| DunkError::Refused("durable project store not configured".into()))?;
        let index = self
            .projects
            .iter()
            .position(|project| project.name == name)
            .ok_or_else(|| DunkError::Refused(format!("no project named {name:?}")))?;
        if self.projects[index].pending_step.is_some() {
            return Err(DunkError::Refused(format!(
                "project {name}: previous step outcome is uncertain; inspect before retry"
            )));
        }
        if self.projects[index].state == State::Stopped {
            return Ok(self.projects[index].clone());
        }
        let mut failures = Vec::new();
        for (role, pane_id) in self.projects[index].agents.clone() {
            let step = format!("stop_agent_{pane_id}");
            begin_external_step(&mut self.projects[index], &step, store).await?;
            let result = terminal
                .close("pane", &pane_id)
                .map(|()| {
                    self.projects[index]
                        .agents
                        .retain(|(_, pane)| *pane != pane_id);
                    format!("closed {role}={pane_id}")
                })
                .map_err(|error| format!("{role}={pane_id}: {error}"));
            if let Err(error) =
                finish_external_step(&mut self.projects[index], &step, result, store).await
            {
                failures.push(error.to_string());
            }
        }
        if failures.is_empty() {
            if let Some(workspace) = self.projects[index].workspace_id.clone() {
                begin_external_step(&mut self.projects[index], "stop_workspace", store).await?;
                let result = terminal
                    .close("workspace", &workspace)
                    .map(|()| {
                        self.projects[index].workspace_id = None;
                        format!("closed {workspace}")
                    })
                    .map_err(|error| format!("{workspace}: {error}"));
                if let Err(error) =
                    finish_external_step(&mut self.projects[index], "stop_workspace", result, store)
                        .await
                {
                    failures.push(error.to_string());
                }
            }
        }
        if !failures.is_empty() {
            return Err(DunkError::Refused(format!(
                "project {name}: stop incomplete ({}); calling project_stop again retries what is left",
                failures.join("; ")
            )));
        }
        self.projects[index].state = State::Stopped;
        let kept = self.projects[index].dir.display().to_string();
        durable_log(
            &mut self.projects[index],
            "stopped",
            true,
            format!("agents and workspace closed; directory kept at {kept}"),
            store,
        )
        .await?;
        Ok(self.projects[index].clone())
    }

    /// Compatibility entry point for callers without approved budget input. It fails closed;
    /// durable project creation must use `spawn_from_sources`.
    pub fn spawn(
        &mut self,
        _terminal: &mut dyn Terminal,
        _root: &Path,
        _request: &Request,
        _authority: &Authority,
        _usage: &Usage,
    ) -> Result<Project, DunkError> {
        Err(DunkError::Refused("project budget not configured".into()))
    }

    #[cfg(test)]
    fn spawn_unbudgeted_for_test(
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
            if existing.state == State::Stopped {
                // Its steps are all "done", so resuming would flip it to Live with no agent
                // running: a stopped project must never look started.
                return Err(DunkError::Refused(format!(
                    "project {name:?} was stopped by the owner (its directory is kept); start a new project under another name"
                )));
            }
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
                pending_step: None,
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
        let result = run_steps(
            project,
            terminal,
            request,
            authority,
            self.reporter.as_deref(),
        );
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

async fn retain_uncertain_external_step(
    project: &mut Project,
    step: &str,
    detail: impl Into<String>,
    store: &newprojects::ProjectStore,
) -> Result<(), DunkError> {
    // Keep `pending_step` set. A caller or restart must inspect the external effect before any
    // replay; the event records the observed pane/report context without claiming completion.
    durable_log(project, &format!("{step}_uncertain"), false, detail, store).await
}

async fn durable_log(
    project: &mut Project,
    step: &str,
    ok: bool,
    detail: impl Into<String>,
    store: &newprojects::ProjectStore,
) -> Result<(), DunkError> {
    log(project, step, ok, detail);
    let sequence = project.events.len().saturating_sub(1) as i64;
    let event = project.events.last().cloned();
    store
        .save(project.clone(), event.map(|event| (sequence, event)))
        .await
}

async fn begin_external_step(
    project: &mut Project,
    step: &str,
    store: &newprojects::ProjectStore,
) -> Result<(), DunkError> {
    project.pending_step = Some(step.to_owned());
    durable_log(
        project,
        &format!("{step}_started"),
        true,
        "outcome pending",
        store,
    )
    .await
}

async fn finish_external_step(
    project: &mut Project,
    step: &str,
    result: Result<String, String>,
    store: &newprojects::ProjectStore,
) -> Result<(), DunkError> {
    let pending = project.pending_step.clone();
    project.pending_step = None;
    match result {
        Ok(detail) => {
            if let Err(error) = durable_log(project, step, true, detail, store).await {
                project.pending_step = pending;
                return Err(error);
            }
            Ok(())
        }
        Err(message) => {
            if let Err(error) = durable_log(project, step, false, message.clone(), store).await {
                project.pending_step = pending;
                return Err(error);
            }
            Err(DunkError::Refused(format!(
                "project {}: step {step} failed: {message}",
                project.name
            )))
        }
    }
}

fn canonical_project_root(root: &Path) -> Result<PathBuf, DunkError> {
    let canonical = std::fs::canonicalize(root).map_err(|error| {
        DunkError::Refused(format!(
            "project root {} is unavailable: {error}",
            root.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(DunkError::Refused(format!(
            "project root {} is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn project_path_present(path: &Path) -> Result<bool, DunkError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(DunkError::Refused(format!(
            "cannot inspect project path {}: {error}",
            path.display()
        ))),
    }
}

fn validate_owned_project_dir(root: &Path, name: &str, project: &Project) -> Result<(), DunkError> {
    let expected = root.join(name);
    if project.dir != expected {
        return Err(DunkError::Refused(format!(
            "project {name}: persisted directory {} does not match approved root {}",
            project.dir.display(),
            expected.display()
        )));
    }
    if project_path_present(&project.dir)? {
        let actual = std::fs::canonicalize(&project.dir).map_err(|error| {
            DunkError::Refused(format!(
                "project {name}: cannot verify persisted directory {}: {error}",
                project.dir.display()
            ))
        })?;
        if actual != expected {
            return Err(DunkError::Refused(format!(
                "project {name}: persisted directory resolves outside its approved root"
            )));
        }
    } else if project.done("scaffolded") {
        return Err(DunkError::Refused(format!(
            "project {name}: durably scaffolded directory is missing"
        )));
    }
    Ok(())
}

async fn run_steps_durable<T: ProjectTerminal + ?Sized>(
    project: &mut Project,
    terminal: &mut T,
    request: &Request,
    authority: &Authority,
    reporter: Option<&dyn Reporter>,
    registrar: Option<&dyn ProjectAgentRegistrar>,
    store: &newprojects::ProjectStore,
) -> Result<(), DunkError> {
    if project.pending_step.is_some() {
        return Err(DunkError::Refused(format!(
            "project {}: previous step outcome is uncertain; inspect before retry",
            project.name
        )));
    }
    if !project.done("validated") {
        durable_log(
            project,
            "validated",
            true,
            format!("template {}", request.template),
            store,
        )
        .await?;
    }
    if !project.done("scaffolded") {
        begin_external_step(project, "scaffolded", store).await?;
        let result =
            scaffold(&project.dir, &project.brief).map(|()| project.dir.display().to_string());
        finish_external_step(project, "scaffolded", result, store).await?;
    }
    if !project.done("workspace") {
        begin_external_step(project, "workspace", store).await?;
        let cwd = project.dir.display().to_string();
        let result = match terminal.create_workspace(Some(&project.name), Some(&cwd), false) {
            Ok(workspace) => {
                project.workspace_id = Some(workspace.workspace_id.clone());
                Ok(workspace.workspace_id)
            }
            Err(error) if registrar.is_some() => {
                let message = error.to_string();
                retain_uncertain_external_step(
                    project,
                    "workspace",
                    format!("workspace creation result uncertain: {message}"),
                    store,
                )
                .await?;
                return Err(DunkError::Refused(format!(
                    "project {}: workspace creation outcome is uncertain; inspect before retry ({message})",
                    project.name
                )));
            }
            Err(error) => Err(error.to_string()),
        };
        finish_external_step(project, "workspace", result, store).await?;
    }
    start_project_agent_durable(
        project,
        terminal,
        store,
        "writer_agent",
        "writer",
        &authority.writer_argv,
        registrar,
    )
    .await?;
    start_project_agent_durable(
        project,
        terminal,
        store,
        "reviewer_agent",
        "reviewer",
        &authority.reviewer_argv,
        registrar,
    )
    .await?;
    if !project.done("reported") {
        let panes: Vec<String> = project
            .agents
            .iter()
            .map(|(role, pane)| format!("{role}={pane}"))
            .collect();
        if let Some(reporter) = reporter {
            begin_external_step(project, "reported", store).await?;
            let brief: String = project
                .brief
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(200)
                .collect();
            let body = format!(
                "project {} is live\ndirectory: {}\nagents: {}\nbrief: {}",
                project.name,
                project.dir.display(),
                panes.join(", "),
                brief
            );
            match reporter.report(&project.name, &body) {
                Ok(id) => {
                    finish_external_step(
                        project,
                        "reported",
                        Ok(format!("sent {id}; {}", panes.join(","))),
                        store,
                    )
                    .await?;
                }
                Err(error) => {
                    retain_uncertain_external_step(
                        project,
                        "reported",
                        format!(
                            "report result uncertain: {error}; would carry {}",
                            panes.join(",")
                        ),
                        store,
                    )
                    .await?;
                    return Err(DunkError::Refused(format!(
                        "project {}: report outcome is uncertain; inspect before retry ({error})",
                        project.name
                    )));
                }
            }
        } else {
            durable_log(
                project,
                "reported",
                false,
                format!(
                    "pending: not sent (owner mailbox not wired); would carry {}",
                    panes.join(",")
                ),
                store,
            )
            .await?;
        }
    }
    Ok(())
}

async fn start_project_agent_durable<T: ProjectTerminal + ?Sized>(
    project: &mut Project,
    terminal: &mut T,
    store: &newprojects::ProjectStore,
    step: &str,
    role: &str,
    argv: &[String],
    registrar: Option<&dyn ProjectAgentRegistrar>,
) -> Result<(), DunkError> {
    if project.done(step) {
        return Ok(());
    }
    if let Some((_, pane_id)) = project.agents.iter().find(|(existing, _)| existing == role) {
        return Err(DunkError::Refused(format!(
            "project {}: {role} pane {pane_id} exists without a completed durable role step; inspect before retry",
            project.name
        )));
    }
    if project.agents.len() >= MAX_AGENTS_PER_PROJECT {
        let reason = format!("{MAX_AGENTS_PER_PROJECT} agents already (MAX_AGENTS_PER_PROJECT)");
        durable_log(project, step, false, reason.clone(), store).await?;
        return Err(DunkError::Refused(format!(
            "project {}: step {step} failed: {reason}",
            project.name
        )));
    }
    if argv.is_empty() {
        let reason = format!("no launch command for the {role}");
        durable_log(project, step, false, reason.clone(), store).await?;
        return Err(DunkError::Refused(format!(
            "project {}: step {step} failed: {reason}",
            project.name
        )));
    }
    begin_external_step(project, step, store).await?;
    let name = format!("{}-{role}", project.name);
    let spec = AgentSpec {
        name: name.clone(),
        argv: argv.to_vec(),
        workspace_id: project.workspace_id.clone(),
        cwd: Some(project.dir.display().to_string()),
        label: Some(name.clone()),
        ..AgentSpec::default()
    };
    let result = match terminal.start_agent(&spec) {
        Err(error) if registrar.is_some() => {
            let message = error.to_string();
            retain_uncertain_external_step(
                project,
                step,
                format!("{role} start result uncertain: {message}"),
                store,
            )
            .await?;
            return Err(DunkError::Refused(format!(
                "project {}: {role} start outcome is uncertain; inspect before retry ({message})",
                project.name
            )));
        }
        Err(error) => Err(error.to_string()),
        Ok(pane) => {
            // Persist the returned pane identity before role registration. If registration or
            // its receipt fails, a retry sees the pane and refuses to start a duplicate.
            project.agents.push((role.into(), pane.pane_id.clone()));
            store.save(project.clone(), None).await?;
            let registered = match registrar {
                Some(registrar) => registrar
                    .register(role, &pane, "codex", "gpt-6-luna", &name)
                    .map(|()| {
                        format!(
                            "pane={} role={role} provider=codex model=gpt-6-luna registered argv={}",
                            pane.pane_id,
                            spec.argv.join(" ")
                        )
                    }),
                None => Ok(pane.pane_id.clone()),
            };
            match registered {
                Ok(detail) => Ok(detail),
                Err(message) if registrar.is_some() => {
                    retain_uncertain_external_step(
                        project,
                        step,
                        format!(
                            "pane={} role registration uncertain: {message}",
                            pane.pane_id
                        ),
                        store,
                    )
                    .await?;
                    return Err(DunkError::Refused(format!(
                        "project {}: {role} pane {} role registration is uncertain; inspect before retry ({message})",
                        project.name, pane.pane_id
                    )));
                }
                Err(message) => Err(message),
            }
        }
    };
    finish_external_step(project, step, result, store).await
}

#[cfg(test)]
fn fail(project: &mut Project, step: &str, message: String) -> Result<(), DunkError> {
    log(project, step, false, message.clone());
    Err(DunkError::Refused(format!(
        "project {}: step {step} failed: {message}",
        project.name
    )))
}

#[cfg(test)]
fn run_steps(
    project: &mut Project,
    terminal: &mut dyn Terminal,
    request: &Request,
    authority: &Authority,
    reporter: Option<&dyn Reporter>,
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
    for (step, role, argv) in [
        ("writer_agent", "writer", &authority.writer_argv),
        ("reviewer_agent", "reviewer", &authority.reviewer_argv),
    ] {
        if project.done(step) {
            continue;
        }
        if project.agents.len() >= MAX_AGENTS_PER_PROJECT {
            return fail(
                project,
                step,
                format!("{MAX_AGENTS_PER_PROJECT} agents already (MAX_AGENTS_PER_PROJECT)"),
            );
        }
        if argv.is_empty() {
            return fail(project, step, format!("no launch command for the {role}"));
        }
        let name = format!("{}-{role}", project.name);
        let spec = AgentSpec {
            name: name.clone(),
            argv: argv.clone(),
            workspace_id: project.workspace_id.clone(),
            cwd: Some(project.dir.display().to_string()),
            label: Some(name),
            ..AgentSpec::default()
        };
        match terminal.start_agent(&spec) {
            Ok(pane) => {
                project.agents.push((role.into(), pane.pane_id.clone()));
                log(project, step, true, pane.pane_id);
            }
            Err(error) => return fail(project, step, error.to_string()),
        }
    }
    if !project.done("reported") {
        let panes: Vec<String> = project
            .agents
            .iter()
            .map(|(r, p)| format!("{r}={p}"))
            .collect();
        match reporter {
            // NOTHING IS SENT when no mailbox is wired: the step is logged as NOT ok with what
            // the report will carry, so a Live project never claims a report it did not make.
            None => log(
                project,
                "reported",
                false,
                format!(
                    "pending: not sent (owner mailbox not wired); would carry {}",
                    panes.join(",")
                ),
            ),
            Some(reporter) => {
                // The brief is the desk's text: only its first line, capped, goes in the report.
                let brief: String = project
                    .brief
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect();
                let body = format!(
                    "project {} is live\ndirectory: {}\nagents: {}\nbrief: {}",
                    project.name,
                    project.dir.display(),
                    panes.join(", "),
                    brief
                );
                match reporter.report(&project.name, &body) {
                    Ok(id) => log(
                        project,
                        "reported",
                        true,
                        format!("sent {id}; {}", panes.join(",")),
                    ),
                    // A failed report FAILS the step: the project stays Partial, and the next
                    // call retries only this step (every earlier step is already done).
                    Err(error) => {
                        return fail(
                            project,
                            "reported",
                            format!("{error}; would carry {}", panes.join(",")),
                        )
                    }
                }
            }
        }
    }
    Ok(())
}

/// Step 3: `~/workspace/<name>`, `git init`, `BRIEF.md`, a `dunkingsheep/` dir.
fn scaffold(dir: &Path, brief: &str) -> Result<(), String> {
    // Reserve the name atomically. A concurrent foreign directory is never adopted or modified.
    std::fs::create_dir(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    std::fs::create_dir(dir.join("dunkingsheep"))
        .map_err(|e| format!("create {}/dunkingsheep: {e}", dir.display()))?;
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
        Authority::project_template(approved)
    }

    fn root(tag: &str) -> crate::testsupport::Scratch<PathBuf> {
        let scratch = crate::testsupport::dir(&format!("projects-{tag}"));
        crate::testsupport::Scratch::new(scratch.path().to_path_buf(), scratch)
    }

    #[test]
    fn names_are_slugs() {
        assert_eq!(validate_name(" my-app ").unwrap(), "my-app");
        for bad in ["", "a", "-x", "x-", "Up", "a b", "../etc", &"a".repeat(40)] {
            assert!(validate_name(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn approved_template_starts_one_luna_low_writer_and_one_luna_medium_reviewer() {
        let authority = Authority::project_template(true);
        assert_eq!(
            authority.writer_argv,
            vec![
                "codex".to_string(),
                "--no-daemon".to_string(),
                "--model".to_string(),
                "gpt-6-luna".to_string(),
                "--config".to_string(),
                "model_reasoning_effort=low".to_string()
            ]
        );
        assert_eq!(
            authority.reviewer_argv,
            vec![
                "codex".to_string(),
                "--no-daemon".to_string(),
                "--model".to_string(),
                "gpt-6-luna".to_string(),
                "--config".to_string(),
                "model_reasoning_effort=medium".to_string()
            ]
        );
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
            .spawn_unbudgeted_for_test(
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
            .spawn_unbudgeted_for_test(
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
        assert_eq!(2, live.agents.len());
        let steps: Vec<&str> = live.events.iter().map(|e| e.step.as_str()).collect();
        assert_eq!(steps, STEPS.to_vec());
    }

    #[test]
    fn spawn_is_idempotent_by_name() {
        let (mut term, mut projects, root) = (FakeTerminal::new(), Projects::new(), root("idem"));
        let first = projects
            .spawn_unbudgeted_for_test(
                &mut term,
                &root,
                &request("app"),
                &authority(true),
                &Usage::Percent(10.0),
            )
            .unwrap();
        let calls = term.layout_calls.len();
        let again = projects
            .spawn_unbudgeted_for_test(
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
            .spawn_unbudgeted_for_test(
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
            .spawn_unbudgeted_for_test(
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
                .spawn_unbudgeted_for_test(
                    &mut term,
                    &root,
                    &request(&format!("p{index}x")),
                    &authority(true),
                    &Usage::Percent(1.0),
                )
                .unwrap();
        }
        let err = projects
            .spawn_unbudgeted_for_test(
                &mut term,
                &root,
                &request("extra"),
                &authority(true),
                &Usage::Percent(1.0),
            )
            .unwrap_err();
        assert!(err.to_string().contains("MAX_LIVE_PROJECTS"), "{err}");
        // A separate registry, so the refusal below is the usage gate and not the live cap.
        let hot = Projects::new().spawn_unbudgeted_for_test(
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
            .spawn_unbudgeted_for_test(
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
        // before a restart) was overwritten and project agents were started in it.
        let (mut term, mut projects, root) = (FakeTerminal::new(), Projects::new(), root("exists"));
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(root.join("app/BRIEF.md"), "PRECIOUS").unwrap();
        let refused = projects
            .spawn_unbudgeted_for_test(
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
        let _ = projects.spawn_unbudgeted_for_test(
            &mut term,
            &root,
            &request("app"),
            &authority(true),
            &Usage::Percent(10.0),
        );
        std::fs::write(root.join("app/BRIEF.md"), "edited by hand").unwrap();
        term.fail_layout = None;
        projects
            .spawn_unbudgeted_for_test(
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
            writer_argv: vec![],
            reviewer_argv: vec![],
        };
        let err = projects
            .spawn_unbudgeted_for_test(
                &mut term,
                &root,
                &request("app"),
                &nobody,
                &Usage::Percent(10.0),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("no launch command for the writer"), "{err}");
    }

    #[test]
    fn missing_owner_reporter_does_not_mark_the_report_sent() {
        // A missing mailbox adapter is visible and never becomes a successful report event.
        let (mut term, mut projects, root) =
            (FakeTerminal::new(), Projects::new(), root("reported"));
        let live = projects
            .spawn_unbudgeted_for_test(
                &mut term,
                &root,
                &request("app"),
                &authority(true),
                &Usage::Percent(10.0),
            )
            .unwrap();
        assert_eq!(live.state, State::Live);
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
                .spawn_unbudgeted_for_test(
                    &mut term,
                    &root,
                    &request(&format!("w{index}x")),
                    &authority(false),
                    &Usage::Percent(1.0),
                )
                .unwrap();
        }
        let refused = projects
            .spawn_unbudgeted_for_test(
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
                    .spawn_unbudgeted_for_test(
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
            .spawn_unbudgeted_for_test(
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

    #[derive(Debug, Clone)]
    struct FakeReporter {
        sent: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
        fail: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl FakeReporter {
        fn new() -> FakeReporter {
            FakeReporter {
                sent: Default::default(),
                fail: Default::default(),
            }
        }
    }

    impl Reporter for FakeReporter {
        fn report(&self, project: &str, body: &str) -> Result<String, String> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("owner mailbox unavailable".into());
            }
            self.sent
                .lock()
                .unwrap()
                .push((project.into(), body.into()));
            Ok(format!("i{}", self.sent.lock().unwrap().len()))
        }
    }

    #[test]
    fn a_wired_reporter_sends_the_report_and_the_step_says_so() {
        let reporter = FakeReporter::new();
        let (mut term, mut projects, root) = (
            FakeTerminal::new(),
            Projects::with_reporter(Box::new(reporter.clone())),
            root("report-ok"),
        );
        let mut req = request("app");
        req.brief = "build a thing\nSECOND LINE must not leak into the report".into();
        let live = projects
            .spawn_unbudgeted_for_test(
                &mut term,
                &root,
                &req,
                &authority(true),
                &Usage::Percent(10.0),
            )
            .unwrap();
        assert_eq!(State::Live, live.state);
        let event = live.events.iter().find(|e| e.step == "reported").unwrap();
        assert!(event.ok, "{}", event.detail);
        assert!(
            event.detail.starts_with("sent i1; writer=") && event.detail.contains("reviewer="),
            "{}",
            event.detail
        );
        let sent = reporter.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "app");
        assert!(
            sent[0].1.contains("project app is live")
                && sent[0].1.contains("agents: writer=")
                && sent[0].1.contains("reviewer="),
            "{}",
            sent[0].1
        );
        assert!(
            sent[0].1.contains("brief: build a thing") && !sent[0].1.contains("SECOND LINE"),
            "{}",
            sent[0].1
        );
    }

    #[test]
    fn a_failed_report_leaves_the_project_partial_and_the_retry_sends_only_the_report() {
        let reporter = FakeReporter::new();
        reporter
            .fail
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (mut term, mut projects, root) = (
            FakeTerminal::new(),
            Projects::with_reporter(Box::new(reporter.clone())),
            root("report-retry"),
        );
        let err = projects
            .spawn_unbudgeted_for_test(
                &mut term,
                &root,
                &request("app"),
                &authority(true),
                &Usage::Percent(10.0),
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("step reported failed") && err.contains("owner mailbox unavailable"),
            "{err}"
        );
        assert_eq!(State::Partial, projects.get("app").unwrap().state);
        let agents_before = projects.get("app").unwrap().agents.len();
        reporter
            .fail
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let live = projects
            .spawn_unbudgeted_for_test(
                &mut term,
                &root,
                &request("app"),
                &authority(true),
                &Usage::Percent(10.0),
            )
            .unwrap();
        assert_eq!(State::Live, live.state);
        assert_eq!(
            agents_before,
            live.agents.len(),
            "no second agent was started by the retry"
        );
        assert_eq!(
            live.events
                .iter()
                .filter(|e| matches!(e.step.as_str(), "writer_agent" | "reviewer_agent") && e.ok)
                .count(),
            2
        );
        assert_eq!(
            reporter.sent.lock().unwrap().len(),
            1,
            "exactly one report went out"
        );
        // and a live project is idempotent: asking again sends nothing more
        projects
            .spawn_unbudgeted_for_test(
                &mut term,
                &root,
                &request("app"),
                &authority(true),
                &Usage::Percent(10.0),
            )
            .unwrap();
        assert_eq!(reporter.sent.lock().unwrap().len(), 1);
    }

    fn live_project(
        tag: &str,
    ) -> (
        FakeTerminal,
        Projects,
        crate::testsupport::Scratch<PathBuf>,
        Project,
    ) {
        let (mut term, mut projects, root) = (FakeTerminal::new(), Projects::new(), root(tag));
        let live = projects
            .spawn_unbudgeted_for_test(
                &mut term,
                &root,
                &request("app"),
                &authority(true),
                &Usage::Percent(10.0),
            )
            .unwrap();
        (term, projects, root, live)
    }

    #[test]
    fn the_owner_can_stop_a_project_and_its_directory_is_kept() {
        let (mut term, mut projects, root, live) = live_project("stop-ok");
        let pane = live.agents[0].1.clone();
        let workspace = live.workspace_id.clone().unwrap();
        let stopped = projects.stop(&mut term, "app", true).unwrap();
        assert_eq!(State::Stopped, stopped.state);
        assert!(stopped.agents.is_empty() && stopped.workspace_id.is_none());
        assert!(
            term.layout_calls
                .contains(&("close".to_string(), pane.clone())),
            "the agent pane was closed"
        );
        assert!(
            term.layout_calls
                .contains(&("close".to_string(), workspace)),
            "the workspace was closed"
        );
        assert!(
            root.join("app/BRIEF.md").exists() && root.join("app/.git").exists(),
            "the owner's work is kept"
        );
        let steps: Vec<(&str, bool)> = stopped
            .events
            .iter()
            .rev()
            .take(3)
            .map(|e| (e.step.as_str(), e.ok))
            .collect();
        assert_eq!(
            steps,
            [
                ("stopped", true),
                ("stop_workspace", true),
                ("stop_agent", true)
            ]
        );
        // idempotent: nothing more is closed
        let calls = term.layout_calls.len();
        assert_eq!(
            State::Stopped,
            projects.stop(&mut term, "app", true).unwrap().state
        );
        assert_eq!(calls, term.layout_calls.len());
    }

    #[test]
    fn project_stop_is_owner_only_and_refuses_before_closing_anything() {
        let (mut term, mut projects, _root, _live) = live_project("stop-gate");
        let calls = term.layout_calls.len();
        let err = projects
            .stop(&mut term, "app", false)
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("project_stop is owner-only"), "{err}");
        assert_eq!(calls, term.layout_calls.len(), "nothing was closed");
        assert_eq!(State::Live, projects.get("app").unwrap().state);
        assert!(projects
            .stop(&mut term, "nope", true)
            .unwrap_err()
            .to_string()
            .contains("no project named \"nope\""));
    }

    #[test]
    fn a_failed_close_leaves_the_project_live_and_a_retry_closes_what_is_left() {
        let (mut term, mut projects, _root, live) = live_project("stop-retry");
        term.fail_layout = Some("tmux gone".into());
        let err = projects
            .stop(&mut term, "app", true)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("stop incomplete") && err.contains("tmux gone"),
            "{err}"
        );
        let after = projects.get("app").unwrap();
        assert_eq!(
            State::Live,
            after.state,
            "a project whose agent could not be closed is not stopped"
        );
        assert_eq!(
            after.agents,
            vec![live.agents[0].clone()],
            "the failed writer close remains recorded while the reviewer close completes"
        );
        assert!(after.events.iter().any(|e| e.step == "stop_agent" && !e.ok));
        let stopped = projects.stop(&mut term, "app", true).unwrap();
        assert_eq!(State::Stopped, stopped.state);
    }

    #[test]
    fn a_stopped_name_is_never_silently_revived_and_frees_its_live_slot() {
        let (mut term, mut projects, root, _live) = live_project("stop-revive");
        projects.stop(&mut term, "app", true).unwrap();
        let err = projects
            .spawn_unbudgeted_for_test(
                &mut term,
                &root,
                &request("app"),
                &authority(true),
                &Usage::Percent(10.0),
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("was stopped by the owner") && err.contains("another name"),
            "{err}"
        );
        assert_eq!(
            State::Stopped,
            projects.get("app").unwrap().state,
            "still stopped, no agent started"
        );
        // a stopped project no longer counts against MAX_LIVE_PROJECTS
        for index in 0..MAX_LIVE_PROJECTS {
            projects
                .spawn_unbudgeted_for_test(
                    &mut term,
                    &root,
                    &request(&format!("p{index}x")),
                    &authority(true),
                    &Usage::Percent(1.0),
                )
                .unwrap();
        }
    }

    #[test]
    fn a_project_still_waiting_for_the_owner_stops_without_touching_the_terminal() {
        let (mut term, mut projects, root) =
            (FakeTerminal::new(), Projects::new(), root("stop-waiting"));
        projects
            .spawn_unbudgeted_for_test(
                &mut term,
                &root,
                &request("app"),
                &authority(false),
                &Usage::Percent(10.0),
            )
            .unwrap();
        let stopped = projects.stop(&mut term, "app", true).unwrap();
        assert_eq!(State::Stopped, stopped.state);
        assert!(term.layout_calls.is_empty());
    }
}
