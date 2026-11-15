import Timestamp from './Timestamp';
import React, { useEffect, useRef, useState } from 'react';
import { Message } from '@chatscope/chat-ui-kit-react';
import { EditorContent, useEditor } from '@tiptap/react';
import StarterKit from '@tiptap/starter-kit';
import MessageBody from './MessageBody';
import TurndownService from 'turndown';
import { messageDeliveryLabel } from './delivery-label.js';
import { api } from './DeskShared';

const td = new TurndownService({ headingStyle: 'atx', codeBlockStyle: 'fenced' });

function Composer({ channel, onSend, pending, setPending, disabled }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [empty, setEmpty] = useState(true);
  const fileRef = useRef(null);
  const sendingRef = useRef(false);
  const sendRef = useRef(null);
  const editor = useEditor({ extensions: [StarterKit], content: '', onUpdate: ({ editor }) => setEmpty(editor.isEmpty), editorProps: {
    attributes: { class: 'editor', 'aria-label': 'Message text' },
    handleKeyDown: (_view, event) => {
      if (event.key === 'Enter' && !event.shiftKey && !event.isComposing && event.keyCode !== 229) {
        event.preventDefault();
        if (!event.repeat) sendRef.current?.();
        return true;
      }
      return false;
    },
  } });
  const command = (name) => { editor?.chain().focus()[name]().run(); };
  async function attach(files) {
    setError('');
    for (const file of Array.from(files).slice(0, 10 - pending.length)) {
      const form = new FormData(); form.append('file', file);
      try { const item = await api('/uploads', { method: 'POST', body: form }); setPending(items => [...items, item]); }
      catch (e) { setError(e.message); }
    }
  }
  async function send() {
    if (disabled || sendingRef.current) return;
    const body = td.turndown(editor?.getHTML() || '').trim();
    if (!body && !pending.length) return;
    sendingRef.current = true; setBusy(true); setError('');
    try {
      await onSend({ channel, body, attachment_ids: pending.map(a => a.id) });
      editor?.commands.clearContent(); setPending([]);
    } catch (e) { setError(e.message); } finally { sendingRef.current = false; setBusy(false); }
  }
  sendRef.current = send;
  return <section className="composer" aria-label="Write message">
    {pending.length > 0 && <div className="pending-files">{pending.map(file => <span key={file.id} className="file-pill">📎 {file.filename}<button aria-label={`Remove ${file.filename}`} onClick={() => setPending(items => items.filter(x => x.id !== file.id))}>×</button></span>)}</div>}
    <div className="formatting-controls" role="group" aria-label="Message formatting">
      <button title="Bold" aria-label="Bold" onClick={() => command('toggleBold')}><svg className="composer-icon" viewBox="0 0 24 24" aria-hidden="true"><path d="M7 4h6a4 4 0 0 1 0 8H7V4Zm0 8h7a4 4 0 0 1 0 8H7v-8Z" /></svg></button>
      <button title="Italic" aria-label="Italic" onClick={() => command('toggleItalic')}><svg className="composer-icon" viewBox="0 0 24 24" aria-hidden="true"><path d="M10 4h10M4 20h10M15 4 9 20" /></svg></button>
      <button title="Heading" aria-label="Heading" onClick={() => editor?.chain().focus().toggleHeading({ level: 2 }).run()}><svg className="composer-icon" viewBox="0 0 24 24" aria-hidden="true"><path d="M5 4v16M19 4v16M5 12h14" /></svg></button>
      <button title="Bullet list" aria-label="Bullet list" onClick={() => command('toggleBulletList')}><svg className="composer-icon" viewBox="0 0 24 24" aria-hidden="true"><path d="M9 5h12M9 12h12M9 19h12M3 5h.01M3 12h.01M3 19h.01" /></svg></button>
      <button title="Quote" aria-label="Quote" onClick={() => command('toggleBlockquote')}><svg className="composer-icon" viewBox="0 0 24 24" aria-hidden="true"><path d="M3 5h7v8H3V5Zm7 8c0 4-3 6-6 6M14 5h7v8h-7V5Zm7 8c0 4-3 6-6 6" /></svg></button>
      <button title="Code block" aria-label="Code block" onClick={() => command('toggleCodeBlock')}><svg className="composer-icon" viewBox="0 0 24 24" aria-hidden="true"><path d="m7 6-5 6 5 6m10-12 5 6-5 6M14 3l-4 18" /></svg></button>
    </div>
    <div className="composer-input" onDragOver={e => e.preventDefault()} onDrop={e => { e.preventDefault(); attach(e.dataTransfer.files); }}>
      <EditorContent editor={editor} />
      {empty && <span className="composer-placeholder" data-hint={disabled ? 'Agent unavailable' : 'Enter to send · Shift+Enter for a new line · files stay on this host'} />}
    </div>
    <button className="primary send" aria-label={busy ? 'Sending message' : 'Send message'} disabled={busy || disabled} onClick={send}>{busy ? 'Sending…' : <>Send<span className="send-decoration" aria-hidden="true"> ↗</span></>}</button>
    <button className="attach" title="Attach images or files" aria-label="Attach images or files" onClick={() => fileRef.current?.click()}><svg className="composer-icon" viewBox="0 0 24 24" aria-hidden="true"><path d="m8 13 7-7a3 3 0 0 1 4 4l-9 9a5 5 0 0 1-7-7l10-10" /></svg><span className="attach-label">Attach</span></button>
    <input ref={fileRef} type="file" multiple hidden onChange={e => { attach(e.target.files); e.target.value = ''; }} />
    {error && <span className="form-error" role="alert">{error}</span>}
  </section>;
}

function MessageCard({ item, highlight, onSendNow, onInspect, frontDesk = false }) {
  const owner = item.role === 'owner';
  const senderLabel = owner ? 'YOU' : item.role === 'agent' ? 'AGENT' : 'SOURCE UNKNOWN';
  const senderClass = owner ? 'owner' : item.role === 'agent' ? 'agent' : 'unattributed';
  const deliveryLabel = messageDeliveryLabel(item);
  const waitingSeconds = Math.max(0, Math.floor((Date.now() - new Date(item.created_at).getTime()) / 1000));
  const waitingMinutes = Math.floor(waitingSeconds / 60);
  const heldForDraft = item.delivery_detail?.includes('someone is typing') || item.delivery_detail?.includes('input box contains a draft');
  return <div className={`message-wrap ${senderClass} ${highlight ? 'highlight' : ''}`} id={`message-${item.id}`}>
    <div className="message-meta"><strong>{senderLabel}</strong><Timestamp value={item.created_at} />{owner && <span className={`delivery ${item.delivery}`} title={item.delivery_detail}>{deliveryLabel}</span>}</div>
    <Message model={{ message: '', direction: owner ? 'outgoing' : 'incoming', position: 'single', sender: senderLabel }}>
      <Message.CustomContent><MessageBody body={item.body} frontDesk={frontDesk} />
        {item.attachments?.length > 0 && <div className="attachments">{item.attachments.map(a => <div key={a.id} className="attachment-card">{['image/png','image/jpeg','image/gif','image/webp'].includes(a.mime) && <a href={`/api/uploads/${a.id}`} target="_blank" rel="noreferrer"><img src={`/api/uploads/${a.id}?preview=true`} loading="lazy" alt={a.filename} /></a>}<a href={`/api/uploads/${a.id}`} target="_blank" rel="noreferrer">📎 {a.filename}</a></div>)}</div>}</Message.CustomContent>
    </Message>
    {owner && item.delivery === 'queued' && <div className={`queue-help ${heldForDraft && waitingMinutes >= 2 ? 'long-wait' : ''}`} role={heldForDraft && waitingMinutes >= 2 ? 'alert' : undefined}><span>{heldForDraft && waitingMinutes >= 2 ? 'Action needed: the lane input box has an unfinished draft' : heldForDraft ? 'Waiting for the lane input box to clear' : (item.delivery_detail || 'Waiting for guarded delivery')} · {waitingMinutes < 1 ? 'under 1 min' : `${waitingMinutes} min`}</span>{heldForDraft && <button onClick={() => onSendNow(item)}>Send now</button>}</div>}
    {owner && item.delivery === 'inbox' && waitingSeconds >= 30 && <div className={`queue-help ${waitingMinutes >= 2 ? 'long-wait' : ''}`} role={waitingMinutes >= 2 ? 'alert' : undefined}><span>{item.delivery_detail || 'Stored in agent inbox'}; waiting for agent pickup · {waitingMinutes < 1 ? `${waitingSeconds} sec` : `${waitingMinutes} min`}</span></div>}
    {owner && item.delivery === 'failed' && <div className="queue-help"><span>{item.delivery_detail || 'Delivery failed; retrying when transport recovers'}</span></div>}
    {owner && item.delivery === 'uncertain' && <div className="queue-help"><span>Delivery uncertain. Check agent activity before resending.</span><button onClick={onInspect}>Live signal</button></div>}
  </div>;
}

function ForceSendDialog({ item, busy, onClose, onConfirm }) {
  return <div className="modal-backdrop" onMouseDown={onClose}><div className="settings-modal" onMouseDown={e => e.stopPropagation()} role="dialog" aria-modal="true" aria-label="Send queued message now">
    <div className="settings-head"><div><div className="eyebrow">DELIVERY OVERRIDE</div><h2>Send this message now?</h2></div><button className="icon-button" onClick={onClose} aria-label="Close">×</button></div>
    <p className="override-copy">The lane's pane appears to contain an unfinished draft. Sending now may combine your message with that draft. The queued copy will be cancelled first to prevent a duplicate.</p>
    <div className="override-actions"><button onClick={onClose} disabled={busy}>Keep waiting</button><button className="primary" onClick={() => onConfirm(item)} disabled={busy}>{busy ? 'Sending…' : 'Send now'}</button></div>
  </div></div>;
}

function Search({ onClose, onJump }) {
  const [q, setQ] = useState(''); const [results, setResults] = useState([]); const [busy, setBusy] = useState(false);
  const [nextBefore, setNextBefore] = useState(null); const [hasMore, setHasMore] = useState(false);
  useEffect(() => { if (q.trim().length < 2) { setResults([]); setHasMore(false); return; }
    let cancelled = false;
    const timer = setTimeout(async () => { setBusy(true); try { const data = await api(`/search?q=${encodeURIComponent(q.trim())}`); if (!cancelled) { setResults(data.items); setHasMore(data.has_more); setNextBefore(data.next_before); } } catch { if (!cancelled) setResults([]); } finally { if (!cancelled) setBusy(false); } }, 250);
    return () => { cancelled = true; clearTimeout(timer); };
  }, [q]);
  async function loadMore() {
    if (!hasMore || !nextBefore || busy) return;
    setBusy(true);
    try { const data = await api(`/search?q=${encodeURIComponent(q.trim())}&before=${nextBefore}`); setResults(old => [...old, ...data.items]); setHasMore(data.has_more); setNextBefore(data.next_before); }
    finally { setBusy(false); }
  }
  return <div className="modal-backdrop" onMouseDown={onClose}><div className="search-modal" onMouseDown={e => e.stopPropagation()} role="dialog" aria-modal="true" aria-label="Search conversation history">
    <div className="search-head"><input autoFocus placeholder="Search messages across projects…" value={q} onChange={e => setQ(e.target.value)} /><button onClick={onClose}>Esc ×</button></div>
    <div className="search-results">{busy && <div className="muted">Searching…</div>}{!busy && q.length >= 2 && !results.length && <div className="muted">No matching messages</div>}
      {results.map(item => <button key={item.id} className="search-result" onClick={() => { onJump(item); onClose(); }}><span>{item.channel.toUpperCase()} · <Timestamp value={item.created_at} /></span><strong>{item.body.slice(0, 260)}</strong></button>)}
      {hasMore && <button className="search-more" onClick={loadMore} disabled={busy}>{busy ? 'Loading…' : 'More results'}</button>}</div>
  </div></div>;
}

export { Composer, MessageCard };
