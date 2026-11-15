import React, { useRef, useState } from 'react';

// The daemon resolves the recorded peer and confirms parent/root after commit.
export default function InboxReply({ api, parent, onSent, draftValue, onDraftChange }) {
  const [localDraft, setLocalDraft] = useState('');
  const draft = draftValue ?? localDraft;
  const setDraft = onDraftChange ?? setLocalDraft;
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [receipt, setReceipt] = useState('');
  const sending = useRef(false);
  async function send(event) {
    event.preventDefault();
    if (sending.current || !draft.trim() || Array.from(draft).length > 16000) return;
    sending.current = true; setBusy(true); setError(''); setReceipt('');
    try {
      const result = await api('/owner-send', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ text: draft, parent_id: parent.message_id }),
      });
      if (result.outcome !== 'stored' || !/^i[1-9][0-9]*$/.test(result.message_id || '')
        || result.thread?.thread_version !== 1 || result.thread?.parent_id !== parent.message_id
        || result.thread?.root_id !== parent.root_id || result.thread?.conversation_kind !== 'human'
        || result.thread?.sender_kind !== 'human') {
        throw new Error('The reply receipt could not be verified. Check your inbox before sending again.');
      }
      setDraft(''); setReceipt(`Reply stored as ${result.message_id}. This is not a read receipt.`);
      onSent?.();
    } catch (e) { setError(e.message); }
    finally { sending.current = false; setBusy(false); }
  }
  return <form className="inbox-compose" aria-label="Reply to message" onSubmit={send}>
    <h3>Reply to {parent.message_id}</h3>
    <label>Reply<textarea aria-label="Reply message" value={draft} disabled={busy} rows={4} onChange={event => setDraft(event.target.value)} /></label>
    <button type="submit" disabled={busy || !draft.trim() || Array.from(draft).length > 16000}>{busy ? 'Sending…' : 'Send reply'}</button>
    {receipt && <p role="status">{receipt}</p>}
    {error && <p role="alert">{error} Your draft is kept.</p>}
  </form>;
}
