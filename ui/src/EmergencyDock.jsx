import React, { useEffect, useRef, useState } from 'react';

export default function EmergencyDock({ request }) {
  const [open, setOpen] = useState(false);
  const [target, setTarget] = useState('cos');
  const [drafts, setDrafts] = useState({ cos: '', vp: '' });
  const [channels, setChannels] = useState([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [receipt, setReceipt] = useState(null);
  const dialog = useRef(null), input = useRef(null), priorFocus = useRef(null), sending = useRef(false);
  const selected = channels.find(channel => channel.id === target);
  function show() {
    if (!dialog.current?.open) priorFocus.current = document.activeElement;
    setOpen(true);
  }
  useEffect(() => {
    function shortcut(event) {
      if ((event.ctrlKey || event.metaKey) && (event.key === '/' || event.code === 'Slash')) {
        event.preventDefault(); show();
      }
    }
    window.addEventListener('keydown', shortcut);
    return () => window.removeEventListener('keydown', shortcut);
  }, []);
  useEffect(() => {
    if (open) {
      if (!dialog.current.open) dialog.current.showModal();
      input.current.focus();
      let live = true;
      request('/channels').then(data => { if (live) { setChannels(data.channels || []); setError(''); } })
        .catch(e => { if (live) setError(e.message); });
      return () => { live = false; };
    }
    if (dialog.current.open) dialog.current.close();
    priorFocus.current?.focus();
  }, [open, request]);
  async function send(event) {
    event.preventDefault();
    const text = drafts[target].trim();
    if (sending.current || !text || !selected?.can_message) return;
    sending.current = true; setBusy(true); setError(''); setReceipt(null);
    const sentTo = target;
    try {
      const result = await request('/pane-send', { method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ pane: selected.pane_id, channel: sentTo, text }) });
      const outcome = result.outcome ?? result.delivery;
      setReceipt({ target: sentTo, outcome, id: result.id });
      if (outcome !== 'refused') setDrafts(old => ({ ...old, [sentTo]: old[sentTo].trim() === text ? '' : old[sentTo] }));
    } catch (e) { setError(e.message); }
    finally { sending.current = false; setBusy(false); }
  }
  return <>
    <button className="dock-trigger" onClick={show} aria-label="Message CoS / VP" title="Message CoS / VP"><span className="nav-action-label">Message CoS / VP</span><span className="nav-action-icon" aria-hidden="true">✉</span> <kbd>Ctrl+/</kbd></button>
    <dialog className="message-dock" ref={dialog} aria-labelledby="dock-title"
      onCancel={event => { event.preventDefault(); setOpen(false); }} onClose={() => setOpen(false)}>
      <header><h2 id="dock-title">Message CoS or VP</h2><button onClick={() => setOpen(false)} aria-label="Close message dock">Close</button></header>
      <form onSubmit={send}>
        <label>Recipient<select value={target} disabled={busy} onChange={event => { setTarget(event.target.value); setReceipt(null); setError(''); }}>
          <option value="cos">Front Desk</option>
        </select></label>
        <label htmlFor="dock-message">Message</label>
        <textarea id="dock-message" ref={input} rows={5} maxLength={20000} value={drafts[target]}
          onChange={event => setDrafts(old => ({ ...old, [target]: event.target.value }))} />
        {!selected?.can_message && <p className="dash-muted">Recipient messaging is unavailable.</p>}
        {error && <p role="alert" className="dash-error">{error}</p>}
        {receipt && <p role="status">{receipt.outcome}{receipt.id ? ' · #' + receipt.id : ''} · <a href={'/dash/' + receipt.target}>Open conversation</a></p>}
        <button type="submit" disabled={busy || !drafts[target].trim() || !selected?.can_message}>{busy ? 'Sending…' : 'Send message'}</button>
      </form>
    </dialog>
  </>;
}
