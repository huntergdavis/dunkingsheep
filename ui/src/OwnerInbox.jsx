import React, { useEffect, useRef, useState } from 'react';
import MessageBody from './MessageBody';
import InboxCompose from './InboxCompose';
import './owner-inbox.css';

const PAGE_SIZE = 25;

// This is the existing owner conversation projection, not a raw mailbox listing.
// Fetching and selecting messages only performs GETs; no read/ack operation is sent.
export default function OwnerInbox({ api }) {
  const [items, setItems] = useState(null);
  const [selectedId, setSelectedId] = useState(null);
  const [page, setPage] = useState(0);
  const [error, setError] = useState('');
  const [attempt, setAttempt] = useState(0);
  const [cursor, setCursor] = useState(undefined);
  const [history, setHistory] = useState({ busy: false, error: '' });
  const olderBusy = useRef(false);
  const mounted = useRef(true);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  useEffect(() => {
    let live = true;
    async function load() {
      try {
        const data = await api('/owner-thread?limit=200&paged=true');
        if (!Object.hasOwn(data, 'next_before')) throw new Error('History paging is unavailable; a daemon upgrade may be needed.');
        if (!live) return;
        setCursor(previous => previous === undefined ? data.next_before : previous);
        setItems(previous => {
          // Keep rows already loaded if a new arrival pushes one outside the API window.
          const merged = new Map((previous || []).map(item => [item.id, item]));
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
    if (!cursor || olderBusy.current) return;
    olderBusy.current = true;
    setHistory({ busy: true, error: '' });
    try {
      const data = await api(`/owner-thread?limit=200&paged=true&before=${encodeURIComponent(cursor)}`);
      if (!mounted.current) return;
      if (!Object.hasOwn(data, 'next_before') || data.next_before === cursor) throw new Error('History cursor did not advance; refresh the Inbox to restart.');
      setItems(previous => {
        const merged = new Map((previous || []).map(item => [item.id, item]));
        for (const item of data.items || []) merged.set(item.id, item);
        return [...merged.values()].sort((a, b) => new Date(b.created_at) - new Date(a.created_at) || b.id - a.id);
      });
      setCursor(data.next_before);
      setPage(Math.floor((items?.length || 0) / PAGE_SIZE));
      setHistory({ busy: false, error: '' });
    } catch (e) { if (mounted.current) setHistory({ busy: false, error: e.message }); }
    finally { olderBusy.current = false; }
  }
  const rows = items || [];
  const selected = rows.find(item => item.id === selectedId) || rows[0];
  const pages = Math.max(1, Math.ceil(rows.length / PAGE_SIZE));
  const currentPage = Math.min(page, pages - 1);
  return <div className="owner-inbox">
    <header className="owner-inbox-heading">
      <div><h1>Owner inbox</h1><p>Owner conversation, including owner lane mail. Load older messages to browse the full available history.</p></div>
      <a href="/dash/cos?panel=mailboxes">Browse all mailboxes</a>
      <a href="/dash/cos?panel=conversation">Open conversation / send a message</a>
    </header>
    <InboxCompose api={api} onSent={() => setAttempt(value => value + 1)} />
    {error && <div className="dash-error" role="alert">{items ? 'Updates unavailable. Showing previously loaded messages. ' : 'Inbox unavailable. '}{error} <button onClick={() => setAttempt(value => value + 1)}>Retry</button></div>}
    {items !== null && <div className="owner-inbox-history">
      {cursor && <button disabled={history.busy} onClick={loadOlder}>{history.busy ? 'Loading older messages…' : 'Load older messages'}</button>}
      {cursor === null && <p role="status">Start of conversation reached · {rows.length} messages loaded</p>}
      {history.error && <p role="alert">Older messages unavailable. {history.error} <button onClick={loadOlder}>Retry older messages</button></p>}
    </div>}
    {items === null && !error && <p role="status">Loading inbox…</p>}
    {items !== null && !rows.length && <p role="status">No messages in the owner conversation.</p>}
    {rows.length > 0 && <div className="owner-inbox-columns">
      <section className="owner-inbox-list" aria-label="Owner messages">
        <nav className="owner-inbox-pages" aria-label="Message pages">
          <button disabled={currentPage === 0} onClick={() => setPage(currentPage - 1)}>Newer</button>
          <span>Page {currentPage + 1} of {pages} · {rows.length} loaded</span>
          <button disabled={currentPage + 1 >= pages} onClick={() => setPage(currentPage + 1)}>Older</button>
        </nav>
        <ul>{rows.slice(currentPage * PAGE_SIZE, (currentPage + 1) * PAGE_SIZE).map(item => <li key={item.id}>
          <button className="owner-inbox-row" aria-current={selected?.id === item.id ? 'true' : undefined} onClick={() => setSelectedId(item.id)}>
            <strong>{item.sender || (item.role === 'owner' ? 'You' : 'Unknown sender')}</strong>
            <time dateTime={item.created_at}>{new Date(item.created_at).toLocaleString()}</time>
            <span className="owner-inbox-preview">{item.body?.trim().split('\n').find(line => line.trim()) || '(Empty message)'}</span>
            <small>{item.role || 'unknown'} · {item.transport || 'transport unknown'}</small>
          </button>
        </li>)}</ul>
      </section>
      {selected && <article className="owner-inbox-reader" aria-label="Selected message" key={selected.id}>
        <header><h2>{selected.sender || (selected.role === 'owner' ? 'You' : 'Unknown sender')}</h2>
          <p><time dateTime={selected.created_at}>{new Date(selected.created_at).toLocaleString()}</time> · {selected.message_id || selected.id}</p>
          <p>{selected.role || 'unknown'} · {selected.transport || 'transport unknown'}{selected.reply_to ? ` · In reply to ${selected.reply_to}` : ''}</p>
        </header>
        <MessageBody body={selected.body || ''} frontDesk />
      </article>}
    </div>}
  </div>;
}
