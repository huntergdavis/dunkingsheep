import React, { useEffect, useRef, useState } from 'react';
import { laneModel } from './lane-model.js';
import { laneDisplayName } from './lane-label.js';
import ProjectTerminal from './LazyProjectTerminal';
import LaneUsage from './LaneUsage';
import { informationHref } from './project-info.js';
import { nextTerminalList, OWNER_TERMINAL_NAME, terminalActionForLane, terminalListReason } from './project-terminal-targets.js';
import { agoText, laneMailText, unroutedSection } from './project-fleet.js';
import { requestedPane } from './dash-route.js';
import { api, DashShell } from './DeskShared';

function Projects({ standalone = false }) {
  const [roster, setRoster] = useState(null);
  const [error, setError] = useState('');
  const [selected, setSelected] = useState('all');
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
  // t5043: the provider usage readings the operations board already computes; read-only.
  const [budget, setBudget] = useState(null);
  useEffect(() => {
    let live = true;
    const load = () => api('/ops-board')
      .then(board => { if (live) setBudget(board?.budget || null); })
      .catch(() => { if (live) setBudget(null); });
    load(); const timer = setInterval(load, 60000);
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
  const ordered = [...groups.values()].sort((a, b) => a.title.localeCompare(b.title));
  const effectiveSelected = selected === 'all' || groups.has(selected) ? selected : 'all';
  return <DashShell standalone={standalone} view="projects"><div className="dash-body">
    <div className="ops-heading"><div><div className="eyebrow">PROJECTS</div><h1>Projects</h1><p>Live lanes grouped by their recorded membership.</p></div></div>
    {error && <div role="alert">Project roster unavailable: {error}</div>}
    {!roster && !error && <p>Loading projects…</p>}
    {terminalList.state === 'error' && <p role="status">{terminalListReason(terminalList.status)}.</p>}
    {deepLinkPane && <p role="status">Selected lane {deepLinkPane}{terminalActionForLane(terminalList, deepLinkPane).reason ? ` · ${terminalActionForLane(terminalList, deepLinkPane).reason}` : ''}</p>}
    {ownerAction && <section className="project-group"><h2>VP of Agentry</h2><button type="button" onClick={() => setTerminalTarget(ownerAction.target)}>Open VP terminal</button></section>}
    {roster && <><label>Show <select aria-label="Filter projects" value={effectiveSelected} onChange={e => setSelected(e.target.value)}><option value="all">All groups</option>{ordered.map(group => <option key={group.key} value={group.key}>{group.title}</option>)}</select></label>
      {!ordered.length && <p>No live agent lanes are reported.</p>}
      {ordered.filter(group => effectiveSelected === 'all' || group.key === effectiveSelected).map(group => <section className="project-group" key={group.key}><h2>{group.title} <small>({group.lanes.length})</small>{group.project !== undefined && <a className="project-information-link" href={informationHref(group.project)}>Information</a>}</h2><div className="ops-table-wrap"><table><thead><tr><th>Lane</th><th>Activity</th><th>Mail</th><th>Task</th><th>Model</th></tr></thead><tbody>{group.lanes.map(lane => <tr key={lane.pane_id}><td className={lane.pane_id === deepLinkPane ? 'lane-deep-link' : undefined}><a aria-current={lane.pane_id === deepLinkPane ? 'true' : undefined} href={`/dash/projects?pane=${encodeURIComponent(lane.pane_id)}`}>{laneDisplayName(lane, lane.pane_id)}</a><small> {lane.pane_id}</small><LaneUsage lane={lane} budget={budget} />{(action => action.target
        ? <button type="button" onClick={() => setTerminalTarget(action.target)}>{action.label}</button>
        : <small className="lane-terminal-reason"> {action.reason}</small>)(terminalActionForLane(terminalList, lane.pane_id))}</td><td>{lane.state || 'Unknown'}</td><td>{(mail => <span title={mail.reason || undefined}>{mail.text}</span>)(laneMailText(fleet, lane.pane_id))}</td><td>{lane.task ? [lane.task.id, lane.task.title].filter(Boolean).join(' · ') : 'Unknown'}</td><td data-lane-model={lane.pane_id}>{(model => <>{model.text}<small> {model.detail}</small></>)(laneModel(lane.model))}</td></tr>)}</tbody></table></div></section>)}
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
