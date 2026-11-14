import React, { useEffect, useState } from 'react';
import MessageBody from './MessageBody';
import './mailbox-browser.css';

function stamp(seconds) {
  const date = new Date(seconds * 1000);
  return Number.isNaN(date.getTime()) ? 'Time unavailable' : date.toLocaleString();
}
function groupFor(row) {
  if (row.mapping === 'unavailable') return 'Membership unavailable';
  if (!row.lanes?.length) return 'Unmapped / history';
  const groups = [...new Set(row.lanes.map(lane => lane.membership?.scope === 'project' && lane.membership.project
    ? `Project: ${lane.membership.project}` : lane.membership?.scope === 'global' ? 'Global lanes' : 'Unknown / stale membership'))];
  return groups.length === 1 ? groups[0] : 'Multiple memberships';
}

export default function MailboxBrowser({ api }) {
  const [mailbox, setMailbox] = useState('owner');
  const [mailboxes, setMailboxes] = useState([]);
  const [indexCursor, setIndexCursor] = useState(null);
  const [index, setIndex] = useState({ state: 'loading' });
  const [before, setBefore] = useState(null);
  const [trail, setTrail] = useState([]);
  const [summary, setSummary] = useState({ state: 'loading' });
  const [selectedId, setSelectedId] = useState(null);
  const [includeAgent, setIncludeAgent] = useState(false);
  const [refresh, setRefresh] = useState(0);
  const [body, setBody] = useState({ state: 'idle' });
  const pageKey = JSON.stringify([mailbox, before, refresh]);
  useEffect(() => {
    let live = true;
    setIndex(previous => ({ ...previous, state: 'loading', error: '' }));
    api(`/mailboxes?limit=50${indexCursor ? `&after=${encodeURIComponent(indexCursor)}` : ''}`)
      .then(data => {
        if (!live) return;
        setMailboxes(previous => {
          const merged = new Map((indexCursor ? previous : []).map(row => [row.mailbox, row]));
          for (const row of data.mailboxes || []) merged.set(row.mailbox, row);
          return [...merged.values()];
        });
        setIndex({ state: 'ready', next: data.next_after, total: data.total });
      }).catch(e => { if (live) setIndex(previous => ({ ...previous, state: 'error', error: e.message })); });
    return () => { live = false; };
  }, [api, indexCursor, refresh]);
  useEffect(() => {
    let live = true;
    setSummary({ key: pageKey, state: 'loading' });
    api(`/mailbox-messages?mailbox=${encodeURIComponent(mailbox)}&limit=50${before ? `&before=${encodeURIComponent(before)}` : ''}`)
      .then(data => { if (live) setSummary({ key: pageKey, state: 'ready', rows: data.messages || [], next: data.next_before }); })
      .catch(e => { if (live) setSummary({ key: pageKey, state: e.status === 502 ? 'unavailable' : 'error', error: e.status === 502 ? 'Mailbox browsing is temporarily unavailable. A daemon upgrade may be needed; try Refresh or use Owner conversation.' : e.message }); });
    return () => { live = false; };
  }, [api, pageKey]);
  const rows = summary.key === pageKey && summary.state === 'ready' ? summary.rows : [];
  const selected = rows.find(row => row.message_id === selectedId && (includeAgent || row.audience !== 'agent'))
    || rows.find(row => row.audience !== 'agent') || (includeAgent ? rows[0] : null);
  const readKey = selected ? JSON.stringify([mailbox, selected.mailbox, selected.message_id, includeAgent, refresh]) : null;
  useEffect(() => {
    let live = true;
    if (!selected) { setBody({ state: 'idle' }); return () => { live = false; }; }
    setBody({ key: readKey, state: 'loading' });
    api(`/mailbox-message?mailbox=${encodeURIComponent(selected.mailbox)}&message_id=${encodeURIComponent(selected.message_id)}${includeAgent ? '&include_agent=true' : ''}`)
      .then(data => {
        if (!live) return;
        if (!data.message || data.message.message_id !== selected.message_id || data.message.mailbox !== selected.mailbox) throw new Error('Message unavailable');
        setBody({ key: readKey, state: 'ready', message: data.message });
      }).catch(e => { if (live) setBody({ key: readKey, state: e.status === 502 ? 'unavailable' : 'error', error: e.status === 502 ? 'Mailbox browsing is temporarily unavailable. A daemon upgrade may be needed; try Refresh or use Owner conversation.' : e.message }); });
    return () => { live = false; };
  }, [api, readKey]);
  const visibleBody = body.key === readKey ? body : { state: 'loading' };
  const groups = new Map();
  for (const row of mailboxes.filter(row => row.mailbox !== 'owner')) {
    const group = groupFor(row);
    if (!groups.has(group)) groups.set(group, []);
    groups.get(group).push(row);
  }
  const agentRows = rows.filter(row => row.audience === 'agent');
  function chooseMailbox(key) { setMailbox(key); setBefore(null); setTrail([]); setSelectedId(null); }
  function reload() { setIndexCursor(null); setBefore(null); setTrail([]); setRefresh(value => value + 1); }
  function messageRow(row) {
    return <li key={row.message_id}><button className="mailbox-message-row" disabled={row.audience === 'agent' && !includeAgent}
      aria-current={selected?.message_id === row.message_id ? 'true' : undefined} onClick={() => setSelectedId(row.message_id)}>
      <strong>{row.subject || '(No subject)'}</strong><span>{row.sender || 'Unknown sender'}</span>
      <small>{stamp(row.created_at)} · {row.state}{row.audience === 'agent' ? ' · agent-only' : ''}</small>
    </button></li>;
  }
  return <div className="mailbox-browser">
    <header className="mailbox-browser-heading"><div><h1>Mailboxes</h1><p>Browse history without marking messages read.</p></div><a href="/dash/cos">Owner conversation</a><button onClick={reload}>Refresh</button></header>
    <label className="mailbox-agent-opt-in"><input type="checkbox" checked={includeAgent} onChange={event => setIncludeAgent(event.target.checked)} /> Include agent-only mail in this browser</label>
    <div className="mailbox-browser-columns">
      <nav className="mailbox-directory" aria-label="Mailboxes">
        <button className="mailbox-folder" aria-current={mailbox === 'owner' ? 'true' : undefined} onClick={() => chooseMailbox('owner')}>Owner inbox</button>
        {[...groups.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([group, folders]) => <section key={group}><h2>{group}</h2>{folders.map(row => <button className="mailbox-folder" key={row.mailbox} aria-current={mailbox === row.mailbox ? 'true' : undefined} onClick={() => chooseMailbox(row.mailbox)}>
          <strong>{row.mailbox}</strong><small>{row.lanes?.map(lane => lane.name).filter(Boolean).join(' / ') || row.destination || 'No current lane match'}</small><small>{row.unread ?? '—'} unread · {row.total ?? '—'} messages</small>
        </button>)}</section>)}
        <p>{mailboxes.length} of {index.total ?? '—'} mailboxes loaded</p>
        {index.state === 'loading' && <p role="status">Loading mailboxes…</p>}
        {index.state === 'error' && <p role="alert">Mailboxes unavailable: {index.error}</p>}
        {index.next && <button disabled={index.state === 'loading'} onClick={() => setIndexCursor(index.next)}>Load more mailboxes</button>}
      </nav>
      <section className="mailbox-summaries" aria-label="Message summaries">
        <h2>{mailbox}</h2>
        <nav className="mailbox-page-controls" aria-label="Message pages"><button disabled={!trail.length} onClick={() => { setBefore(trail[trail.length - 1]); setTrail(trail.slice(0, -1)); setSelectedId(null); }}>Newer</button><span>Page {trail.length + 1}</span><button disabled={summary.key !== pageKey || summary.state !== 'ready' || !summary.next} onClick={() => { setTrail([...trail, before]); setBefore(summary.next); setSelectedId(null); }}>Older</button></nav>
        {(summary.key !== pageKey || summary.state === 'loading') && <p role="status">Loading messages…</p>}
        {summary.key === pageKey && summary.state === 'unavailable' && <p role="status">{summary.error}</p>}
        {summary.key === pageKey && summary.state === 'error' && <p role="alert">Messages unavailable: {summary.error}</p>}
        {summary.key === pageKey && summary.state === 'ready' && !rows.length && <p>No messages in this mailbox.</p>}
        <ul>{rows.filter(row => row.audience !== 'agent').map(messageRow)}</ul>
        {agentRows.length > 0 && <details open={includeAgent}><summary>Agent-only messages ({agentRows.length})</summary>{!includeAgent && <p>Enable agent-only mail above to read these messages.</p>}<ul>{agentRows.map(messageRow)}</ul></details>}
      </section>
      <article className="mailbox-reader" aria-label="Selected mailbox message">
        {!selected && <p>Select a message to read it.</p>}
        {selected && <><header><h2>{selected.subject || '(No subject)'}</h2><p>{selected.sender || 'Unknown sender'} · {stamp(selected.created_at)}</p><p>{selected.message_id} · {selected.state} · {selected.mailbox}{selected.audience === 'agent' ? ' · agent-only' : ''}</p></header>
          {visibleBody.state === 'loading' && <p role="status">Loading message…</p>}
          {visibleBody.state === 'unavailable' && <p role="status">{visibleBody.error}</p>}
          {visibleBody.state === 'error' && <p role="alert">{visibleBody.error}</p>}
          {visibleBody.state === 'ready' && <><MessageBody body={visibleBody.message.body} frontDesk />{visibleBody.message.attachments?.length > 0 && <div><h3>Attachments</h3><ul>{visibleBody.message.attachments.map(file => <li key={file.id}>{file.filename || file.id} · {file.size} bytes</li>)}</ul><p>Attachment downloads are not available here yet.</p></div>}</>}
        </>}
      </article>
    </div>
  </div>;
}
