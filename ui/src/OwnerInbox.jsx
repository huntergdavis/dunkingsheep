import React, { useEffect, useRef, useState } from 'react';
import MessageBody from './MessageBody';
import InboxCompose from './InboxCompose';
import InboxReply from './InboxReply';
import { HISTORICAL_REASONS, INBOX_VIEWS, messageKindLabels, validateInboxPage } from './inbox-views.js';
import './owner-inbox.css';

const PAGE_SIZE = 25;
const senderLabel = item => item.thread_version === 1 && item.sender_kind === 'human' ? 'You' : item.sender || (item.role === 'owner' ? 'You' : 'Unknown sender');

// This is the existing owner conversation projection, not a raw mailbox listing.
// Fetching and selecting messages only performs GETs; no read/ack operation is sent.
export default function OwnerInbox({ api }) {
  const [sourceFilter, setSourceFilter] = useState('human_threads');
  const [requestedRoot, setRequestedRoot] = useState(null);
  const viewButtons = useRef({});
  const restoreFocus = useRef(false);
  useEffect(() => {
    if (restoreFocus.current) viewButtons.current[sourceFilter]?.focus();
    restoreFocus.current = false;
  }, [sourceFilter]);
  const controls = <nav className="owner-inbox-views" aria-label="Inbox views">
    {Object.entries(INBOX_VIEWS).map(([name, view]) => <button key={name} type="button"
      ref={node => { viewButtons.current[name] = node; }} aria-pressed={sourceFilter === name}
      onClick={() => { restoreFocus.current = true; setRequestedRoot(null); setSourceFilter(name); }}>{view.label}</button>)}
  </nav>;
  // A view change unmounts old bodies; late responses cannot cross into another view.
  return <OwnerInboxThread key={sourceFilter} api={api} sourceFilter={sourceFilter} controls={controls} requestedRoot={requestedRoot} openVpThread={root => { setRequestedRoot(root); setSourceFilter('vp_threads'); }} />;
}

function OwnerInboxThread({ api, sourceFilter, controls, requestedRoot, openVpThread }) {
  const [wantedRoot, setWantedRoot] = useState(requestedRoot);
  const view = INBOX_VIEWS[sourceFilter];
  const [items, setItems] = useState(null);
  const [selectedId, setSelectedId] = useState(null);
  const [page, setPage] = useState(0);
  const [error, setError] = useState('');
  const [attempt, setAttempt] = useState(0);
  const [cursor, setCursor] = useState(undefined);
  const [pagingAvailable, setPagingAvailable] = useState(null);
  const [history, setHistory] = useState({ busy: false, error: '' });
  const olderBusy = useRef(false);
  const mounted = useRef(true);
  const revision = useRef(null);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  async function readWindow(before = null) {
    const data = await api(`/owner-thread?limit=200&paged=true&filter=${sourceFilter}${before ? `&before=${encodeURIComponent(before)}` : ''}`);
    return validateInboxPage(data, sourceFilter);
  }

  useEffect(() => {
    let live = true;
    async function load() {
      try {
        const data = await readWindow();
        if (!live) return;
        if (sourceFilter !== 'historical' && revision.current !== null && data.view_revision < revision.current) return;
        const changed = sourceFilter !== 'historical' && revision.current !== null && revision.current !== data.view_revision;
        revision.current = data.view_revision;
        if (changed) {setCursor(data.next_before);setPage(0);setSelectedId(null);}
        setPagingAvailable(data.pagingAvailable);
        setCursor(previous => !data.pagingAvailable ? undefined : previous === undefined ? data.next_before : previous);
        setItems(previous => {
          // Keep rows already loaded if a new arrival pushes one outside the API window.
          const merged = new Map((changed ? [] : previous || []).map(item => [item.id, item]));
          for (const item of data.items || []) merged.set(item.id, item);
          return [...merged.values()].sort((a, b) => new Date(b.created_at) - new Date(a.created_at) || b.id - a.id);
        });
        const newest = [...(data.items || [])].sort((a, b) => new Date(b.created_at) - new Date(a.created_at) || b.id - a.id)[0];
        setSelectedId(previous => previous ?? newest?.id ?? null);
        setError('');
      } catch (e) { if (live) setError(e.message); }
    }
    load();
    const timer = setInterval(load, 5000);
    return () => { live = false; clearInterval(timer); };
  }, [api, attempt]);
  async function loadOlder() {
    if (!pagingAvailable || !cursor || olderBusy.current) return;
    olderBusy.current = true;
    setHistory({ busy: true, error: '' });
    try {
      const data = await readWindow(cursor);
      if (!mounted.current) return;
      if (sourceFilter !== 'historical' && data.view_revision !== revision.current) {
        setItems(null);setCursor(undefined);setSelectedId(null);setPage(0);setAttempt(value => value + 1);
        throw new Error('The conversation changed. Refreshing this inbox.');
      }
      if (data.pagingAvailable && data.next_before === cursor) throw new Error('History cursor did not advance; reload this page to restart.');
      setItems(previous => {
        const merged = new Map((previous || []).map(item => [item.id, item]));
        for (const item of data.items || []) merged.set(item.id, item);
        return [...merged.values()].sort((a, b) => new Date(b.created_at) - new Date(a.created_at) || b.id - a.id);
      });
      setPagingAvailable(data.pagingAvailable);
      setCursor(data.pagingAvailable ? data.next_before : undefined);
      if (data.pagingAvailable) setPage(Math.floor((items?.length || 0) / PAGE_SIZE));
      setHistory({ busy: false, error: '' });
    } catch (e) { if (mounted.current) setHistory({ busy: false, error: e.message }); }
    finally { olderBusy.current = false; }
  }
  const rows = items || [];
  const selected = wantedRoot ? rows.find(item => item.id === selectedId && item.root_id === wantedRoot) || rows.find(item => item.root_id === wantedRoot) : rows.find(item => item.id === selectedId) || rows[0];
  const thread = selected?.thread_version === 1 ? rows.filter(row => row.root_id === selected.root_id).sort((a,b) => new Date(a.created_at)-new Date(b.created_at) || a.id-b.id) : [];
  const pages = Math.max(1, Math.ceil(rows.length / PAGE_SIZE));
  const currentPage = Math.min(page, pages - 1);
  return <div className="owner-inbox">
    <header className="owner-inbox-heading">
      <div><h1>{view.label}</h1><p>{view.description}</p></div>
      <a href="/dash/cos?panel=mailboxes">Browse all mailboxes</a>
      <a href="/dash/cos?panel=conversation">Open conversation / send a message</a>
    </header>
    {controls}
    <InboxCompose api={api} onSent={() => setAttempt(value => value + 1)} />
    {error && <div className="dash-error" role="alert">{items ? 'Updates unavailable. Showing previously loaded messages. ' : 'Inbox unavailable. '}{error} <button onClick={() => setAttempt(value => value + 1)}>Retry</button></div>}
    {items !== null && <div className="owner-inbox-history">
      {pagingAvailable === false && <p role="status">Full-history paging is unavailable. Showing up to 200 recent messages and any history already loaded.</p>}
      {pagingAvailable && cursor && <button disabled={history.busy} onClick={loadOlder}>{history.busy ? 'Loading older messages…' : 'Load older messages'}</button>}
      {pagingAvailable && cursor === null && <p role="status">Start of conversation reached · {rows.length} messages loaded</p>}
      {history.error && <p role="alert">Older messages unavailable. {history.error} <button onClick={loadOlder}>Retry older messages</button></p>}
    </div>}
    {items === null && !error && <p role="status">Loading inbox…</p>}
    {items !== null && !rows.length && <p role="status">No messages in this inbox yet.</p>}
    {wantedRoot && !selected && items !== null && <p role="status">Conversation {wantedRoot} is not loaded. Load older messages to find its recorded history.</p>}
    {rows.length > 0 && <div className="owner-inbox-columns">
      <section className="owner-inbox-list" aria-label="Inbox messages">
        <nav className="owner-inbox-pages" aria-label="Message pages">
          <button disabled={currentPage === 0} onClick={() => setPage(currentPage - 1)}>Newer</button>
          <span>Page {currentPage + 1} of {pages} · {rows.length} loaded</span>
          <button disabled={currentPage + 1 >= pages} onClick={() => setPage(currentPage + 1)}>Older</button>
        </nav>
        <ul>{rows.slice(currentPage * PAGE_SIZE, (currentPage + 1) * PAGE_SIZE).map(item => <li key={item.id}>
          <button className="owner-inbox-row" aria-current={selected?.id === item.id ? 'true' : undefined} onClick={() => { setWantedRoot(null); setSelectedId(item.id); }}>
            <strong>{senderLabel(item)}</strong>
            <time dateTime={item.created_at}>{new Date(item.created_at).toLocaleString()}</time>
            <span className="owner-inbox-preview">{item.body?.trim().split('\n').find(line => line.trim()) || '(Empty message)'}</span>
            <small>{sourceFilter === 'historical' ? [HISTORICAL_REASONS[item.historical_reason], ...item.historical_labels].join(' · ') : [view.label, ...messageKindLabels(item)].join(' · ')}</small>
          </button>
        </li>)}</ul>
      </section>
      {selected && <article className="owner-inbox-reader" aria-label="Selected message" key={selected.id}>
        <header><h2>{senderLabel(selected)}</h2>
          <p><time dateTime={selected.created_at}>{new Date(selected.created_at).toLocaleString()}</time> · {selected.message_id || selected.id}</p>
          {sourceFilter === 'historical' && <p>{[HISTORICAL_REASONS[selected.historical_reason], ...selected.historical_labels].join(' · ')}</p>}
          {sourceFilter !== 'historical' && <p>{messageKindLabels(selected).join(' · ')}</p>}
          {sourceFilter === 'human_threads' && selected.conversation_kind === 'vp' && <button type="button" onClick={() => openVpThread(selected.root_id)}>Open VP thread {selected.root_id}</button>}
          <p>{view.label}{sourceFilter === 'historical' && selected.reply_to ? ` · Original reply reference: ${selected.reply_to}` : ''}</p>
        </header>
        {selected.thread_version === 1 ? <section aria-label="Conversation thread">
          <h3>Conversation {selected.root_id}</h3>
          <p>Showing loaded messages in this conversation. Load older messages to see earlier history.</p>
          {thread.map(message => <section key={message.message_id} className="owner-thread-message">
            <p>{messageKindLabels(message).join(' · ')}</p>
            <h4>{message.sender_kind === 'human' ? 'You' : message.sender || 'System'} · {message.message_id}</h4>
            {message.parent_id && <p>Reply to {message.parent_id}{!thread.some(row => row.message_id === message.parent_id) ? ' · earlier message is not loaded' : ''}</p>}
            {message.unresolved_parent && <p>The earlier transport message is unavailable. This starts a new conversation.</p>}
            <MessageBody body={message.body || ''} frontDesk />
            {message.message_id !== selected.message_id && <button type="button" onClick={() => setSelectedId(message.id)}>Reply to {message.message_id}</button>}
          </section>)}
          <InboxReply key={selected.message_id} api={api} parent={selected} onSent={() => setAttempt(value => value + 1)} />
        </section> : <><MessageBody body={selected.body || ''} frontDesk /><p>Use Compose message to start a new conversation.</p></>}
      </article>}
    </div>}
  </div>;
}
