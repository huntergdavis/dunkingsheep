import React, { lazy, Suspense, useEffect, useRef, useState } from 'react';
import { laneModel } from './lane-model.js';
import { laneDisplayName } from './lane-label.js';
import ProjectTerminal from './LazyProjectTerminal';
import './ops-consolidation.css';
import LaneUsage from './LaneUsage';
import ResourceSummary from './ResourceSummary';
const TasksView = lazy(() => import('./TasksView'));
const ProjectInformation = lazy(() => import('./ProjectInformation'));
const PoolradReport = lazy(() => import('./PoolradReport'));

import { informationHref } from './project-info.js';
import { nextTerminalList, OWNER_TERMINAL_NAME, terminalActionForLane, terminalListReason } from './project-terminal-targets.js';
import { agoText, laneMailText, unroutedSection } from './project-fleet.js';
import { requestedPane } from './dash-route.js';
import { api, DashShell } from './DeskShared';

function Projects({ standalone = false }) {
  const [roster, setRoster] = useState(null);
  const [error, setError] = useState('');
  const [selected, setSelected] = useState(() => { const p = new URLSearchParams(window.location.search).get('project'); return p ? `project:${p}` : 'all'; });
  const [terminalList, setTerminalList] = useState({ state: 'loading' });
  const [terminalTarget, setTerminalTarget] = useState(null);
  useEffect(() => {
    let active = true;
    // Lanes come and go, so the authorized targets are re-read with them (t4643).
    const load = () => api('/terminals')
      .then(items => { if (active) setTerminalList(previous => nextTerminalList(previous, { items })); })
      .catch(e => { if (active) setTerminalList(previous => nextTerminalList(previous, { status: e.status })); });
    load(); const timer = setInterval(load, 30000);
    return () => { active = false; clearInterval(timer); };
  }, []);
  const ownerAction = (terminalList.targets || []).map(target => terminalActionForLane(terminalList, target.pane_id)).find(action => action.target?.name === OWNER_TERMINAL_NAME);
  // PR924: a legacy pane bookmark opens only an exact server-authorized terminal, once.
  const [deepLinkPane] = useState(() => requestedPane(window.location.search));
  const deepLinkDone = useRef(false);
  useEffect(() => {
    if (deepLinkDone.current || !deepLinkPane || terminalList.state !== 'ready') return;
    deepLinkDone.current = true;
    const action = terminalActionForLane(terminalList, deepLinkPane);
    if (action.target) setTerminalTarget(action.target);
  }, [terminalList, deepLinkPane]);
  useEffect(() => {
    let live = true;
    async function load() {
      try { const data = await api('/projects'); if (live) { setRoster(data); setError(''); } }
      catch (e) { if (live) { setRoster(null); setError(e.message); } }
    }
    load(); const timer = setInterval(load, 10000);
    return () => { live = false; clearInterval(timer); };
  }, []);
  // S5a (t4646): the mail counts and the unrouted-process list come from the pane roster, which
  // the Terminals view used to be the only reader of. A failed read is kept as a state the
  // helpers turn into "Unknown" with the reason, never into zeros.
  // Shared provider and host readings are independent of project reporting.
  const [resources, setResources] = useState(null);
  const budget = resources?.budget;
  const [stores, setStores] = useState(null);
  useEffect(() => { let live = true; api('/project-context').then(data => { if (live) setStores(data); }).catch(() => {}); return () => { live = false; }; }, []);
  useEffect(() => {
    let live = true;
    const load = () => api('/project-resources')
      .then(data => { if (live) setResources(data); })
      .catch(() => { if (live) setResources(null); });
    load(); const timer = setInterval(load, 15000);
    return () => { live = false; clearInterval(timer); };
  }, []);
  const [fleet, setFleet] = useState(null);
  useEffect(() => {
    let live = true;
    const load = () => api('/fleet')
      .then(data => { if (live) setFleet(data); })
      .catch(e => { if (live) setFleet({ state: 'unavailable', error: e.message, rows: [] }); });
    load(); const timer = setInterval(load, 10000);
    return () => { live = false; clearInterval(timer); };
  }, []);
  const unrouted = unroutedSection(fleet);
  const groups = new Map();
  for (const lane of roster?.lanes || []) {
    const membership = lane.membership || {};
    const name = membership.scope === 'project' && typeof membership.project === 'string' ? membership.project : null;
    const key = name !== null ? `project:${name}` : `scope:${membership.scope || 'unknown'}`;
    const title = name !== null ? `Project: ${name}` : ({global: 'Global scope', stale: 'Stale membership', unavailable: 'Membership unavailable', unknown: 'Unknown / unregistered'}[membership.scope] || 'Unknown / unregistered');
    if (!groups.has(key)) groups.set(key, { key, title, lanes: [], ...(name !== null ? { project: name } : {}) });
    groups.get(key).lanes.push(lane);
  }
  for (const item of stores?.projects || []) {
    const key = `project:${item.name}`;
    if (!groups.has(key)) groups.set(key, {key, title:`Project: ${item.name}`, project:item.name, lanes:[]});
  }
  const [requestedProject] = useState(() => new URLSearchParams(window.location.search).get('project'));
  if (requestedProject && !groups.has(`project:${requestedProject}`)) groups.set(`project:${requestedProject}`, {key:`project:${requestedProject}`, title:`Project: ${requestedProject}`, project:requestedProject, lanes:[]});
  const ordered = [...groups.values()].sort((a, b) => a.title.localeCompare(b.title));
  const effectiveSelected = selected === 'all' || groups.has(selected) ? selected : 'all';
  return <DashShell standalone={standalone} view="projects"><div className="dash-body">
    <div className="ops-heading"><div><div className="eyebrow">PROJECTS</div><h1>Projects</h1><p>Agents, work, knowledge, and shared host health.</p></div></div>
    <ResourceSummary resources={resources} />
    <p><a href="/dash/tasks?needs_owner=true">Decisions across all projects</a> · <a href="/dash/tasks">All fleet work</a></p>
    {error && <div role="alert">Project roster unavailable: {error}</div>}
    {!roster && !error && <p>Loading projects…</p>}
    {terminalList.state === 'error' && <p role="status">{terminalListReason(terminalList.status)}.</p>}
    {deepLinkPane && <p role="status">Selected lane {deepLinkPane}{terminalActionForLane(terminalList, deepLinkPane).reason ? ` · ${terminalActionForLane(terminalList, deepLinkPane).reason}` : ''}</p>}
    {ownerAction && <section className="project-group"><h2>VP of Agentry</h2><button type="button" onClick={() => setTerminalTarget(ownerAction.target)}>Open VP terminal</button></section>}
    {(roster || stores) && <><label>Show <select aria-label="Filter projects" value={effectiveSelected} onChange={e => { setSelected(e.target.value); const url = new URL(window.location.href); if(e.target.value.startsWith("project:")) url.searchParams.set("project",e.target.value.slice(8)); else url.searchParams.delete("project"); window.history.replaceState(null,"",url); }}><option value="all">All groups</option>{ordered.map(group => <option key={group.key} value={group.key}>{group.title}</option>)}</select></label>
      {!ordered.length && <p>No live agent lanes are reported.</p>}
      {ordered.filter(group => effectiveSelected === 'all' || group.key === effectiveSelected).map(group => <section className="project-group" key={group.key}><h2>{group.title} <small>({group.lanes.length})</small>{group.project !== undefined && <a className="project-information-link" href={informationHref(group.project)}>Knowledge</a>}</h2><div className="ops-table-wrap"><table><thead><tr><th>Lane</th><th>Activity</th><th>Mail</th><th>Task</th><th>Model</th></tr></thead><tbody>{group.lanes.map(lane => <tr key={lane.pane_id}><td className={lane.pane_id === deepLinkPane ? 'lane-deep-link' : undefined}><a aria-current={lane.pane_id === deepLinkPane ? 'true' : undefined} href={`/dash/projects?pane=${encodeURIComponent(lane.pane_id)}`}>{laneDisplayName(lane, lane.pane_id)}</a><small> {lane.pane_id}</small><LaneUsage lane={lane} budget={budget} />{(action => action.target
        ? <button type="button" onClick={() => setTerminalTarget(action.target)}>{action.label}</button>
        : <small className="lane-terminal-reason"> {action.reason}</small>)(terminalActionForLane(terminalList, lane.pane_id))}</td><td>{lane.state || 'Unknown'}</td><td>{(mail => <span title={mail.reason || undefined}>{mail.text}</span>)(laneMailText(fleet, lane.pane_id))}</td><td>{lane.task ? [lane.task.id, lane.task.title].filter(Boolean).join(' · ') : 'Unknown'}</td><td data-lane-model={lane.pane_id}>{(model => <>{model.text}<small> {model.detail}</small></>)(laneModel(lane.model))}</td></tr>)}</tbody></table></div>
        {group.project !== undefined && <ProjectPanels project={group.project} />}
      </section>)}
    </>}
    <details className="project-unrouted">
      <summary>{unrouted.title}</summary>
      {!unrouted.known ? <p className="dash-muted">{unrouted.reason}</p> : <>
        {unrouted.note && <p className="dash-muted">{unrouted.note}</p>}
        {!unrouted.rows.length && <p>No unrouted agent processes detected.</p>}
        {unrouted.rows.map(row => <p key={row.pid}>
          <strong>{row.project}</strong> · {row.agent} · PID {row.pid}
          <br />{row.reason} · Session write: {agoText(row.last_session_write_s)}
          {row.session_age_reason ? ' · ' + row.session_age_reason : ''}
        </p>)}
      </>}
    </details>
    {terminalTarget && <ProjectTerminal key={`${terminalTarget.name}:${terminalTarget.pane_id}`} target={terminalTarget} onClose={() => setTerminalTarget(null)} />}
  </div></DashShell>;
}
export default Projects;

function ProjectPanels({ project }) {
  const [tab, setTab] = useState('');
  return <div className="project-panels">
    <div className="project-panel-tabs" aria-label={`${project} details`}>
      {['work','knowledge', ...(project === 'poolrad' ? ['report'] : [])].map(name => <button key={name} aria-pressed={tab === name} onClick={() => setTab(tab === name ? '' : name)}>{name === 'work' ? 'Work & decisions' : name === 'knowledge' ? 'Knowledge' : 'Poolrad report'}</button>)}
    </div>
    <Suspense fallback={<p role="status">Loading project details…</p>}>
      {tab === 'work' && <TasksView key={project} api={api} project={project} embedded />}
      {tab === 'knowledge' && <ProjectInformation key={project} api={api} project={project} />}
      {tab === 'report' && <PoolradReport />}
    </Suspense>
  </div>;
}
