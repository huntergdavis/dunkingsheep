import React, { useEffect, useRef, useState } from 'react';
import { inboxOutcomeText, quickMessageRoster, quickMessageTargets } from './quick-message.js';
import './inbox-compose.css';

// Uses the existing send routes. A new owner message is not a threaded reply, and
// a lane recipient is chosen from the current fleet, never inferred from mail.
export default function InboxCompose({ api, onSent }) {
  const [open, setOpen] = useState(false);
  const [recipient, setRecipient] = useState('owner');
  const [fleet, setFleet] = useState(null);
  const [load, setLoad] = useState({ status: 'loading' });
  const [attempt, setAttempt] = useState(0);
  const [draft, setDraft] = useState('');
  const [busy, setBusy] = useState(false);
  const [receipt, setReceipt] = useState('');
  const [error, setError] = useState('');
  const sending = useRef(false);
  const opener = useRef(null);
  useEffect(() => {
    if (!open) return;
    let live = true;
    setLoad({ status: 'loading' });
    setFleet(null);
    api('/fleet').then(data => {
      if (live) { setFleet(data); setLoad({ status: 'ready' }); }
    }).catch(e => { if (live) setLoad({ status: 'failed', error: e.message }); });
    return () => { live = false; };
  }, [api, open, attempt]);
  const targets = quickMessageTargets(fleet);
  const roster = quickMessageRoster(load, fleet);
  const owner = recipient === 'owner';
  const target = targets.find(row => row.pane_id === recipient);
  const blocked = busy || !draft.trim() || Array.from(draft).length > 16000
    || (!owner && (roster.kind !== 'ready' || !target));
  async function send(event) {
    event.preventDefault();
    if (blocked || sending.current) return;
    sending.current = true; setBusy(true); setError(''); setReceipt('');
    try {
      const result = await api(owner ? '/owner-send' : '/pane-inbox', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(owner ? { text: draft } : { pane: recipient, text: draft }),
      });
      if (result.outcome !== 'stored' || typeof result.message_id !== 'string' || !result.message_id) {
        throw new Error('No storage receipt was returned. Check the conversation or mailbox before sending again.');
      }
      setReceipt(owner ? `Stored in owner inbox · ${result.message_id}. This is not a read receipt. ${result.notify?.detail || ''}` : inboxOutcomeText(result, target));
      setDraft(current => current === draft ? '' : current);
      onSent?.();
    } catch (e) { setError(e.message); }
    finally { sending.current = false; setBusy(false); }
  }
  function close() { setOpen(false); opener.current?.focus(); }
  return <section className="inbox-compose" aria-label="Compose inbox message">
    <button ref={opener} aria-expanded={open} onClick={() => setOpen(true)}>Compose message</button>
    {open && <form onSubmit={send}>
      <h2>New inbox message</h2>
      <label>To<select aria-label="Message recipient" value={recipient} disabled={busy} onChange={event => { setRecipient(event.target.value); setReceipt(''); setError(''); }}>
        <option value="owner">Owner conversation</option>
        {!owner && !target && <option value={recipient} disabled>{recipient} · unavailable; choose a recipient</option>}
        {targets.map(row => <option key={row.pane_id} value={row.pane_id}>{row.project} · {row.label}</option>)}
      </select></label>
      {roster.kind !== 'ready' && <p role={roster.kind === 'failed' ? 'alert' : 'status'}>{roster.text} {roster.retry && <button type="button" disabled={busy} onClick={() => setAttempt(value => value + 1)}>Reload recipients</button>}</p>}
      {roster.kind === 'ready' && <button type="button" disabled={busy} onClick={() => setAttempt(value => value + 1)}>Reload recipients</button>}
      <label>Message<textarea aria-label="Inbox message" rows={5} value={draft} disabled={busy} onChange={event => setDraft(event.target.value)} autoFocus /></label>
      <small>{Array.from(draft).length} / 16000 characters</small>
      <div className="inbox-compose-actions"><button type="submit" disabled={blocked}>{busy ? 'Sending…' : 'Send message'}</button><button type="button" disabled={busy} onClick={close}>Close composer</button><a href="/dash/cos?panel=conversation">Follow up in owner conversation</a></div>
      {receipt && <p role="status">{receipt}</p>}
      {error && <p role="alert">{error} Your draft is kept. Check for a stored message before sending again.</p>}
    </form>}
  </section>;
}
