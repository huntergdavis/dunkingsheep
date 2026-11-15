import Timestamp from './Timestamp';
import React, { useEffect, useState } from 'react';
import { taskAge, taskQuery, taskPage, metricNumber } from './tasks-model.js';
import './tasks.css';

function TaskMetrics({ api, revision }) {
  const [data, setData] = useState(null);
  const [error, setError] = useState('');
  useEffect(() => {
    let active = true;
    const controller = new AbortController();
    setData(null); setError('');
    api('/tasks/metrics', { signal: controller.signal }).then(value => {
      if (!value?.current || !value?.throughput || !value?.window) throw new Error('Task metrics unavailable');
      if (active) setData(value);
    }).catch(error => { if (active) setError(error.message); });
    return () => { active = false; controller.abort(); };
  }, [api, revision]);
  return <section className="tasks-metrics" aria-label="Fleet task metrics">
    <h2>Fleet task metrics</h2>
    {error ? <p role="status">Metrics unavailable: {error}</p> : !data ? <p role="status">Loading metrics…</p> : <>
      <dl>{[['Open', data.current.by_status?.open], ['In progress', data.current.by_status?.in_progress], ['Blocked', data.current.by_status?.blocked], ['Completed in 24 hours', data.throughput.completed], ['Claimed in 24 hours', data.throughput.claimed]].map(([label, value]) => <div key={label}><dt>{label}</dt><dd>{metricNumber(value)}</dd></div>)}</dl>
      <small>Current counts cover the fleet queue. Activity: <Timestamp value={data.window.since} seconds /> – <Timestamp value={data.window.until} seconds />.</small>
    </>}
  </section>;
}

function TaskResults({ api, filters }) {
  const [cursors, setCursors] = useState([null]);
  const [page, setPage] = useState(null);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(true);
  const cursor = cursors[cursors.length - 1];
  useEffect(() => {
    let active = true;
    const controller = new AbortController();
    setLoading(true); setPage(null); setError('');
    api(`/tasks?${taskQuery(filters, cursor)}`, { signal: controller.signal }).then(raw => {
      const value = taskPage(raw, filters);
      if (value.next_cursor && cursors.includes(value.next_cursor)) throw new Error('Task page repeated; refresh to start again');
      if (active) setPage(value);
    }).catch(error => { if (active) setError(error.message); }).finally(() => { if (active) setLoading(false); });
    return () => { active = false; controller.abort(); };
  }, [api, filters, cursor]);
  return <section className="tasks-results" aria-label="Task results" aria-busy={loading}>
    <div className="tasks-paging">
      <p aria-live="polite">{loading ? 'Loading tasks…' : page ? `Page ${cursors.length} · ${page.tasks.length} shown${page.total === null ? '' : ` · ${page.total} matching`}` : 'Tasks unavailable'}</p>
      <div><button disabled={loading || cursors.length === 1} onClick={() => setCursors(previous => previous.slice(0, -1))}>Previous page</button><button disabled={loading || !page?.next_cursor || page.legacy_limited} onClick={() => setCursors(previous => [...previous, page.next_cursor])}>Next page</button></div>
    </div>
    {error && <p role="alert">{error}. Use Refresh to try again.</p>}
    {page?.legacy_limited && <p className="tasks-limited" role="status">This daemon shows the first page only. Priority filters apply to that limited result. More tasks may exist; full paging needs a daemon update.</p>}
    {page && !page.tasks.length && <p>{page.legacy_limited ? 'No matching tasks in this first page.' : 'No tasks match these filters.'}</p>}
    {page && !!page.tasks.length && <table className="tasks-table">
      <caption className="tasks-sr">{filters.view === 'closed' ? 'Closed' : 'Active'} fleet tasks</caption>
      <thead><tr><th>Task</th><th>Status</th><th>Priority</th><th>Owner</th><th>Age</th><th>Blocked reason</th></tr></thead>
      <tbody>{page.tasks.map(task => <tr key={task.id}>
        <td data-label="Task"><strong>{task.id}</strong><span>{task.title}</span></td>
        <td data-label="Status">{task.status.replaceAll('_', ' ')}</td>
        <td data-label="Priority">{task.priority}</td>
        <td data-label="Owner">{task.assignees.length ? task.assignees.join(', ') : 'Leader (unassigned)'}</td>
        <td data-label="Age">{taskAge(task.created_at, page.checked_at)}<small><Timestamp value={task.created_at} seconds /></small></td>
        <td data-label="Blocked reason">{task.status === 'blocked' ? task.block_reason || 'No reason recorded' : '—'}</td>
      </tr>)}</tbody>
    </table>}
    {page && <p className="tasks-checked">Checked <Timestamp value={page.checked_at} seconds />. The queue can change between pages.</p>}
  </section>;
}

export default function TasksView({ api }) {
  const [filters, setFilters] = useState({ view: 'active', status: '', priority: '', assignee: '' });
  const [assignee, setAssignee] = useState('');
  const [revision, setRevision] = useState(0);
  const statuses = ['active', 'open', 'in_progress', 'blocked', 'closed', 'done', 'cancelled'];
  const change = (key, value) => setFilters(previous => ({ ...previous, [key]: value }));
  const changeStatus = status => setFilters(previous => ({
    ...previous,
    view: ['closed', 'done', 'cancelled'].includes(status) ? 'closed' : 'active',
    status: ['active', 'closed'].includes(status) ? '' : status,
  }));
  return <div className="tasks-view">
    <header className="tasks-heading"><div><h1>Tasks</h1><p>Current work and closed tasks. Read-only.</p></div><button onClick={() => setRevision(value => value + 1)}>Refresh</button></header>
    <TaskMetrics api={api} revision={revision} />
    <form className="tasks-filters" onSubmit={event => { event.preventDefault(); change('assignee', assignee.trim()); }}>
      <label>Status<select aria-label="Status" value={filters.status || filters.view} onChange={event => changeStatus(event.target.value)}>{statuses.map(status => <option key={status} value={status}>{['active', 'closed'].includes(status) ? `All ${status}` : status.replaceAll('_', ' ')}</option>)}</select></label>
      <label>Priority<select aria-label="Priority" value={filters.priority} onChange={event => change('priority', event.target.value)}><option value="">All priorities</option>{['urgent', 'high', 'medium', 'low'].map(priority => <option key={priority} value={priority}>{priority}</option>)}</select></label>
      <label>Assignee<input aria-label="Assignee" value={assignee} maxLength={256} placeholder="Pane ID, e.g. %311" onChange={event => setAssignee(event.target.value)} /></label><button type="submit">Apply assignee</button>
      {filters.assignee && <button type="button" onClick={() => { setAssignee(''); change('assignee', ''); }}>Clear assignee</button>}
    </form>
    <TaskResults key={`${JSON.stringify(filters)}:${revision}`} api={api} filters={filters} />
  </div>;
}
