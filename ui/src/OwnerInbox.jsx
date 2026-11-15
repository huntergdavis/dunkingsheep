import Timestamp from './Timestamp';
import React, { useEffect, useRef, useState } from 'react';
import MessageBody from './MessageBody';
import InboxCompose from './InboxCompose';
import InboxReply from './InboxReply';
import { HISTORICAL_REASONS, INBOX_VIEWS, messageKindLabels, validateThreadPage, mergeThreadHeads, mergeThreadMessages } from './inbox-views.js';
import './owner-inbox.css';

const senderLabel = item => item.sender_kind === 'human' ? 'You' : item.sender || 'Unknown sender';
const labels = (item, historical) => historical ? [HISTORICAL_REASONS[item.historical_reason], ...item.historical_labels] : messageKindLabels(item);
async function readPage(api, view, mode, root = null, cursor = null) {
  const query = new URLSearchParams({ thread_mode: mode, filter: view, limit: mode === 'heads' ? '25' : '50' });
  if (root) query.set('root_id', root);
  if (cursor) query.set('thread_cursor', cursor);
  const data = validateThreadPage(await api(`/owner-thread?${query}`), view, mode, root);
  if (!cursor && data.reload_from_top) throw new Error('Unexpected conversation reload. Try again.');
  return data;
}

// Selection and GET polling never consume or acknowledge mail.
export default function OwnerInbox({ api }) {
  const [viewName, setView] = useState('human_threads');
  const [requestedRoot, setRequestedRoot] = useState(null);
  const [attempt, setAttempt] = useState(0);
  const [drafts, setDrafts] = useState({});
  const [targets, setTargets] = useState({});
  const buttons = useRef({});
  const focusView = useRef(false);
  useEffect(() => { if (focusView.current) buttons.current[viewName]?.focus(); focusView.current = false; }, [viewName]);
  const view = INBOX_VIEWS[viewName];
  return <div className="owner-inbox">
    <header className="owner-inbox-heading">
      <div><h1>{view.label}</h1><p>{view.description}</p></div>
      <a href="/dash/cos?panel=mailboxes">Browse all mailboxes</a>
      <a href="/dash/cos?panel=conversation">Open conversation / send a message</a>
    </header>
    <nav className="owner-inbox-views" aria-label="Inbox views">
      {Object.entries(INBOX_VIEWS).map(([name, entry]) => <button key={name} type="button" ref={node => { buttons.current[name] = node; }} aria-pressed={viewName === name}
        onClick={() => { focusView.current = true; setRequestedRoot(null); setView(name); }}>{entry.label}</button>)}
    </nav>
    <InboxCompose api={api} onSent={() => setAttempt(value => value + 1)} />
    <ThreadList key={viewName} api={api} viewName={viewName} attempt={attempt} requestedRoot={requestedRoot}
      targets={targets} setTarget={(key, value) => setTargets(previous => ({ ...previous, [key]: typeof value === 'function' ? value(previous[key] ?? null) : value }))}
      drafts={drafts} setDraft={(id, text) => setDrafts(previous => ({ ...previous, [id]: text }))}
      refresh={() => setAttempt(value => value + 1)}
      openVp={root => { setRequestedRoot(root); setView('vp_threads'); }} />
  </div>;
}

function ThreadList({ api, viewName, attempt, requestedRoot, drafts, setDraft, targets, setTarget, refresh, openVp }) {
  const [heads, setHeads] = useState(null);
  const [selectedRoot, setSelectedRoot] = useState(requestedRoot);
  const [cursor, setCursor] = useState(null);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const [olderBusy, setOlderBusy] = useState(false);
  const [restart, setRestart] = useState(0);
  const state = useRef({ live: true, sequence: 0, chain: 0, revision: null, initialized: false, olderBusy: false });
  useEffect(() => { state.current.live = true; return () => { state.current.live = false; state.current.sequence++; state.current.chain++; }; }, []);
  function reset(message) {
    state.current.chain++; state.current.sequence++; state.current.initialized = false;
    setHeads(null); setCursor(null); setNotice(message); setRestart(value => value + 1);
  }
  useEffect(() => {
    let live = true;
    async function load() {
      const sequence = ++state.current.sequence;
      try {
        const data = await readPage(api, viewName, 'heads');
        if (!live || sequence !== state.current.sequence) return;
        const changed = state.current.revision !== null && state.current.revision !== data.view_revision;
        if (state.current.revision !== null && data.view_revision < state.current.revision) return;
        if (changed) { state.current.chain++; state.current.initialized = false; setSelectedRoot(null); }
        state.current.revision = data.view_revision;
        if (!state.current.initialized) { setCursor(data.next_cursor); state.current.initialized = true; }
        setHeads(previous => mergeThreadHeads(changed ? [] : previous || [], data.threads));
        setSelectedRoot(previous => previous ?? data.threads[0]?.thread_id ?? null);
        setError('');
      } catch (e) { if (live && sequence === state.current.sequence) setError(e.message); }
    }
    load(); const timer = setInterval(load, 5000);
    return () => { live = false; clearInterval(timer); };
  }, [api, viewName, attempt, restart]);
  async function older() {
    if (!cursor || state.current.olderBusy) return;
    const chain = state.current.chain;
    const boundary = cursor;
    state.current.olderBusy = true; setOlderBusy(true);
    try {
      const data = await readPage(api, viewName, 'heads', null, boundary);
      if (!state.current.live || chain !== state.current.chain) return;
      if (data.reload_from_top || data.view_revision !== state.current.revision) { reset('List changed, reloading conversations.'); return; }
      if (data.next_cursor === boundary) throw new Error('History did not advance. Refresh this inbox.');
      // Older snapshot heads cannot replace a fresher polled head.
      setHeads(previous => mergeThreadHeads(data.threads, previous || []));
      setCursor(data.next_cursor); setError('');
    } catch (e) { if (state.current.live && chain === state.current.chain) setError(e.message); }
    finally { state.current.olderBusy = false; if (state.current.live) setOlderBusy(false); }
  }
  return <>
    <div className="owner-inbox-history">
      <button type="button" onClick={() => reset('Refreshing conversations.')}>Refresh inbox</button>
      {cursor && <button type="button" disabled={olderBusy} onClick={older}>{olderBusy ? 'Loading older conversations…' : 'Load older conversations'}</button>}
      {notice && <p role="status">{notice}</p>}
      {error && <p role="alert">{heads ? 'Updates unavailable. Showing loaded conversations. ' : 'Inbox unavailable. '}{error} <button type="button" onClick={() => setRestart(value => value + 1)}>Retry</button></p>}
    </div>
    {heads === null && !error && <p role="status">Loading inbox…</p>}
    {heads !== null && !heads.length && <p role="status">No conversations in this inbox yet.</p>}
    {(heads?.length > 0 || selectedRoot) && <div className="owner-inbox-columns">
      <section className="owner-inbox-list" aria-label="Inbox conversations">
        <p className="owner-inbox-pages">{heads?.length || 0} conversations loaded</p>
        <ul>{(heads || []).map(head => <li key={head.thread_id}>
          <button className="owner-inbox-row" aria-current={selectedRoot === head.thread_id ? 'true' : undefined} onClick={() => setSelectedRoot(head.thread_id)}>
            <strong>{senderLabel(head.latest)}</strong><Timestamp value={head.latest.created_at} />
            {head.latest.subject && <span>{head.latest.subject}</span>}
            <span className="owner-inbox-preview">{head.latest.preview.trim().split('\n').find(line => line.trim()) || '(Empty message)'}</span>
            <small>{head.message_count} messages · {head.unread_count ? `${head.unread_count} unread` : 'No unread messages'}</small>
            <small>{labels(head.latest, viewName === 'historical').join(' · ')}</small>
          </button>
        </li>)}</ul>
      </section>
      {selectedRoot && <ThreadReader key={`${selectedRoot}:${state.current.revision}`} api={api} viewName={viewName} root={selectedRoot} attempt={attempt}
        drafts={drafts} setDraft={setDraft} targets={targets} setTarget={setTarget} refresh={refresh} openVp={openVp} />}
    </div>}
  </>;
}

function ThreadReader({ api, viewName, root, attempt, drafts, setDraft, targets, setTarget, refresh, openVp }) {
  const [messages, setMessages] = useState(null);
  const parentId = targets[`${viewName}:${root}`] ?? null;
  const setParentId = value => setTarget(`${viewName}:${root}`, value);
  const [cursor, setCursor] = useState(null);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const [busy, setBusy] = useState(false);
  const [restart, setRestart] = useState(0);
  const state = useRef({ live: true, sequence: 0, chain: 0, initialized: false, revision: null, busy: false });
  useEffect(() => { state.current.live = true; return () => { state.current.live = false; state.current.sequence++; state.current.chain++; }; }, []);
  function reset() {
    state.current.chain++; state.current.sequence++; state.current.initialized = false;
    setMessages(null); setCursor(null); setNotice('Conversation changed, reloading from the newest messages.'); setRestart(value => value + 1);
  }
  useEffect(() => {
    let live = true;
    async function load() {
      const sequence = ++state.current.sequence;
      try {
        const data = await readPage(api, viewName, 'messages', root);
        if (!live || sequence !== state.current.sequence) return;
        if (state.current.revision !== null && data.view_revision < state.current.revision) return;
        const changed = state.current.revision !== null && data.view_revision !== state.current.revision;
        if (changed) { state.current.chain++; state.current.initialized = false; setParentId(null); }
        state.current.revision = data.view_revision;
        if (!state.current.initialized) { setCursor(data.next_cursor); state.current.initialized = true; }
        setMessages(previous => mergeThreadMessages(changed ? [] : previous || [], data.messages));
        setParentId(previous => previous ?? data.messages.at(-1)?.message_id ?? null); setError('');
      } catch (e) { if (live && sequence === state.current.sequence) setError(e.message); }
    }
    load(); const timer = setInterval(load, 5000);
    return () => { live = false; clearInterval(timer); };
  }, [api, viewName, root, attempt, restart]);
  async function older() {
    if (!cursor || state.current.busy) return;
    const chain = state.current.chain; const boundary = cursor;
    state.current.busy = true; setBusy(true);
    try {
      const data = await readPage(api, viewName, 'messages', root, boundary);
      if (!state.current.live || chain !== state.current.chain) return;
      if (data.reload_from_top || data.view_revision !== state.current.revision) { reset(); return; }
      if (data.next_cursor === boundary) throw new Error('History did not advance. Refresh this conversation.');
      setMessages(previous => mergeThreadMessages(data.messages, previous || [])); setCursor(data.next_cursor); setError('');
    } catch (e) { if (state.current.live && chain === state.current.chain) setError(e.message); }
    finally { state.current.busy = false; if (state.current.live) setBusy(false); }
  }
  const parent = messages?.find(message => message.message_id === parentId) || messages?.at(-1);
  return <article className="owner-inbox-reader" aria-label="Selected conversation">
    <header><h2>Conversation {root}</h2><p>{INBOX_VIEWS[viewName].label}</p></header>
    {notice && <p role="status">{notice}</p>}
    {error && <p role="alert">Conversation unavailable. {error} <button type="button" onClick={() => setRestart(value => value + 1)}>Retry conversation</button></p>}
    {messages === null && !error && <p role="status">Loading conversation…</p>}
    {cursor && <button type="button" disabled={busy} onClick={older}>{busy ? 'Loading earlier messages…' : 'Load earlier messages'}</button>}
    {messages !== null && cursor === null && <p role="status">Start of conversation reached · {messages.length} messages loaded</p>}
    <section aria-label="Conversation thread">
      {(messages || []).map(message => <section key={message.message_id} className="owner-thread-message">
        <h3>{senderLabel(message)} · {message.message_id}</h3><Timestamp value={message.created_at} />
        <p>{labels(message, viewName === 'historical').join(' · ')}</p>
        {message.parent_id && <p>Reply to {message.parent_id}{!messages.some(row => row.message_id === message.parent_id) ? ' · earlier message is not loaded' : ''}</p>}
        {message.unresolved_parent && <p>The earlier transport message is unavailable. This starts a new conversation.</p>}
        {viewName === 'historical' && message.reply_to && <p>Original reply reference: {message.reply_to}</p>}
        <MessageBody body={message.body} frontDesk />
        {viewName !== 'historical' && <button type="button" aria-pressed={parent?.message_id === message.message_id} onClick={() => setParentId(message.message_id)}>Reply to {message.message_id}</button>}
        {viewName === 'human_threads' && message.conversation_kind === 'vp' && <button type="button" onClick={() => openVp(root)}>Open VP thread {root}</button>}
      </section>)}
    </section>
    {parent && viewName !== 'historical' && <InboxReply key={parent.message_id} api={api} parent={parent} draftValue={drafts[parent.message_id] || ''} onDraftChange={text => setDraft(parent.message_id, text)} onSent={refresh} />}
    {messages !== null && viewName === 'historical' && <p>Use Compose message to start a new conversation.</p>}
  </article>;
}
