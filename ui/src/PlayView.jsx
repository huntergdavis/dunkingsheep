import React, { useEffect, useRef, useState } from 'react';
import { useMessageScroll, mergeMessages } from './useMessageScroll';
import { CodewheelPicker } from './codewheel-picker.jsx';
import CharacterEditor from './CharacterEditor';
import { DASH_ROUTES } from './dash-route.js';
import { api, notifyNote, DashHeader, DashShell } from './DeskShared';

function PoolradPlay({ vpChannel, standalone = false, embedded = false }) {
  const [status, setStatus] = useState(null);
  const [ownChannel, setOwnChannel] = useState(null);
  const channel = vpChannel || ownChannel;
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [stream, setStream] = useState(0);
  const [chat, setChat] = useState([]);
  const [draft, setDraft] = useState('');
  const [chatBusy, setChatBusy] = useState(false);
  const [chatError, setChatError] = useState('');
  const [chatOutcome, setChatOutcome] = useState('');
  const [code, setCode] = useState(() => { try { return localStorage.getItem('vp-codeword') || ''; } catch { return ''; } });
  const [states, setStates] = useState([]);
  const [saveTag, setSaveTag] = useState('');
  const [stateSlot, setStateSlot] = useState('');
  const [statesBusy, setStatesBusy] = useState(false);
  const [statesError, setStatesError] = useState('');
  const screenRef = useRef(null);
  const inputRevision = useRef(0), actionBusy = useRef(false);
  const chatScroll = useMessageScroll(chat, channel?.id);
  const emulator = status?.emulator;
  const buildScope = emulator?.scope === 'running_session' ? 'This session uses'
    : emulator?.scope === 'next_start' ? 'Next session uses' : null;
  const buildLabel = buildScope && emulator?.kind === 'checkpoint' && emulator.release && emulator.sha256
    ? `${buildScope} the checkpoint build (${emulator.release} ${emulator.sha256.slice(0, 8)})`
    : buildScope && emulator?.kind === 'other'
      ? `${buildScope} another emulator build.` : 'Checkpoint build identity is not available.';
  useEffect(() => {
    let live = true;
    const refresh = () => api('/game/status').then(data => { if (live) setStatus(data); }).catch(e => { if (live) setError(e.message); });
    refresh(); const timer = setInterval(refresh, 5000);
    return () => { live = false; clearInterval(timer); };
  }, []);
  useEffect(() => {
    if (!channel) return;
    let live = true;
    const load = () => api(channel.id === 'cos' ? '/owner-thread?limit=15' : `/messages?channel=${encodeURIComponent(channel.id)}&limit=15`)
      .then(data => { if (live) setChat(old => mergeMessages(old, data.items || [])); })
      .catch(e => { if (live) setChatError(e.message); });
    load(); const timer = setInterval(load, 4000);
    return () => { live = false; clearInterval(timer); };
  }, [channel]);
  useEffect(() => {
    if (vpChannel) return;
    api('/channels').then(data => setOwnChannel((data.channels || []).find(c => c.id === 'cos') || null)).catch(() => {});
  }, [vpChannel]);
  async function refreshStates() {
    try {
      const data = await api('/game/states');
      setStates(data.states || []);
      setStatesError('');
    } catch (e) { setStatesError(e.message); }
  }
  useEffect(() => { refreshStates(); }, [status?.run_name, status?.running]);
  async function saveState() {
    if (statesBusy) return;
    setStatesBusy(true); setStatesError('');
    try {
      await api('/game/savestate', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ tag: saveTag.trim() }) });
      setSaveTag('');
      await refreshStates();
    } catch (e) { setStatesError(e.message); } finally { setStatesBusy(false); }
  }
  async function loadState() {
    if (statesBusy || !stateSlot) return;
    setStatesBusy(true); setStatesError('');
    try {
      await api('/game/loadstate', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ slot: Number(stateSlot) }) });
      setStream(n => n + 1);
    } catch (e) { setStatesError(e.message); } finally { setStatesBusy(false); }
  }
  async function autoCodeWord() {
    if (actionBusy.current) return { ok: false, error: 'Another game action is in progress.' };
    actionBusy.current = true; setBusy(true); setError('');
    try {
      const data = await api('/game/autocodeword', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({}) });
      return { ok: true, ...data };
    } catch (e) { setError(e.message); return { ok: false, error: e.message }; }
    finally { actionBusy.current = false; setBusy(false); }
  }
  async function sendChat(text) {
    const body = (text ?? draft).trim();
    if (!body || !channel) return;
    setChatBusy(true); setChatError('');
    try {
      // The desk dock is the owner mailbox (t1151/t1156): the same route as the desk page.
      const owner = channel.id === 'cos';
      const receipt = owner
        ? await api('/owner-send', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ text: body }) })
        : await api('/pane-send', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ pane: channel.pane_id, channel: channel.id, text: body }) });
      setChatOutcome(owner ? `${receipt.outcome} as ${receipt.message_id}${notifyNote(receipt.notify)}` : (receipt.outcome || receipt.delivery));
      setDraft('');
      const data = await api(owner ? '/owner-thread?limit=15' : `/messages?channel=${encodeURIComponent(channel.id)}&limit=15`);
      chatScroll.jump();
      setChat(old => mergeMessages(old, data.items || []));
    } catch (e) { setChatError(e.message); } finally { setChatBusy(false); }
  }
  async function action(path, body) {
    if (actionBusy.current) return false;
    actionBusy.current = true; inputRevision.current++;
    setBusy(true); setError('');
    try {
      const data = await api(`/game/${path}`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body || {}) });
      if (typeof data.running === 'boolean') { setStatus(data); setStream(n => n + 1); }
      if (data.running) setTimeout(() => screenRef.current?.focus(), 100);
      return true;
    } catch (e) { setError(e.message); return false; }
    finally { actionBusy.current = false; setBusy(false); }
  }
  function keyDown(event) {
    if (actionBusy.current || !status?.running || event.ctrlKey || event.metaKey || event.altKey) return;
    const names = { ArrowUp: 'Up', ArrowDown: 'Down', ArrowLeft: 'Left', ArrowRight: 'Right', Enter: 'Return', ' ': 'space', Backspace: 'BackSpace', PageUp: 'Page_Up', PageDown: 'Page_Down' };
    const key = names[event.key] || (/^[a-z0-9]$/i.test(event.key) ? event.key.toLowerCase() : event.key);
    if (!/^(?:[a-z0-9]|F(?:[1-9]|1[0-2])|Up|Down|Left|Right|Return|Escape|space|BackSpace|Tab|Delete|Home|End|Page_Up|Page_Down)$/.test(key)) return;
    event.preventDefault();
    inputRevision.current++;
    api('/game/key', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ key }) }).catch(e => setError(e.message));
  }
  function sendText(submit) {
    const text = code.trim();
    if (!text) return;
    try { localStorage.setItem('vp-codeword', text); } catch {}
    action('type', { text, submit });
  }
  function sendWord(word) {
    setCode(word);
    try { localStorage.setItem('vp-codeword', word); } catch {}
    return action('type', { text: word, submit: true });
  }
  const content = <>
    <DashHeader eyebrow={DASH_ROUTES.play.eyebrow} title={DASH_ROUTES.play.title}
      subtitle={status?.running ? `Live private run · ${status.run_name}` : status?.run_name ? `Saved run · ${status.run_name}` : 'Ready for a new private run'}
      side={<div className="game-actions">{!status?.running ? <><button onClick={() => action('start', { fresh: false })} disabled={busy}>{busy ? 'Starting…' : status?.run_name ? 'Resume saved run' : 'Start game'}</button>{status?.run_name && <button onClick={() => action('start', { fresh: true })} disabled={busy}>New run</button>}</> : <button onClick={() => action('stop')} disabled={busy}>Stop game</button>}</div>} />
    <div className="game-view" aria-label="Play Poolrad">
    {error && <div className="game-error" role="alert">{error}</div>}
    <div className="game-tools" aria-label="Session build"><strong>{buildLabel}</strong></div>
    <div className="game-stage" ref={screenRef} tabIndex={0} onKeyDown={keyDown} onClick={() => screenRef.current?.focus()} aria-label="Poolrad game screen; click here to control with keyboard">
      {status?.running ? <img key={stream} src={`/api/game/video?v=${stream}`} alt="Live Poolrad game display" draggable="false" /> : <div className="game-placeholder">Start the private DOS game.</div>}
    </div>
    <div className="game-footer"><span>Click the screen, then use your keyboard. Arrow keys move; Enter selects; Escape goes back. Saves stay in the private run on this PC.</span>{status?.running && status.audio && <audio key={stream} controls preload="none" src={`/api/game/audio?v=${stream}`} aria-label="Poolrad game audio" />}</div>
    <div className="game-type">
      <label htmlFor="game-code">Type into game</label>
      <input id="game-code" value={code} onChange={e => setCode(e.target.value)} placeholder="Code word / text (press Enter to send)" onKeyDown={e => { if (e.key === 'Enter') { e.preventDefault(); sendText(true); } }} aria-label="Text to type into the game" />
      <button onClick={() => sendText(true)} disabled={busy || !code.trim()}>Send + Enter</button>
      <button onClick={() => sendText(false)} disabled={busy || !code.trim()}>Send only</button>
      <button onClick={() => { setCode(''); try { localStorage.removeItem('vp-codeword'); } catch {} }} disabled={!code}>Clear</button>
    </div>
    <div className="game-saves" aria-label="Save states">
      <strong>Save states</strong>
      <input value={saveTag} onChange={e => setSaveTag(e.target.value)} placeholder="Tag this save (e.g. Sokal Keep first floor)" maxLength={80} aria-label="Save state tag" disabled={!status?.running || statesBusy} />
      <button onClick={saveState} disabled={!status?.running || statesBusy}>{statesBusy ? 'Working…' : 'Save state'}</button>
      <label>Quick load
        <select value={stateSlot} onChange={e => setStateSlot(e.target.value)} disabled={!status?.running || statesBusy || states.length === 0} aria-label="Save state to load">
          <option value="">{states.length ? 'Choose a save…' : 'No save states yet'}</option>
          {states.map(s => <option key={s.slot} value={s.slot}>{`${s.slot}. ${s.tag} — ${new Date(s.created).toLocaleString()}`}</option>)}
        </select>
      </label>
      <button onClick={loadState} disabled={!status?.running || statesBusy || !stateSlot}>Load state</button>
      {statesError && <span className="game-error" role="alert">{statesError}</span>}
    </div>
    {status?.state_archive?.status === 'verified' && status.state_archive.verified_count > 0 && <div className="game-footer" role="note">States from earlier sessions are archived. We cannot verify which build wrote them.</div>}
    <CodewheelPicker screenRef={screenRef} inputRevision={inputRevision} running={Boolean(status?.running)} runKey={status?.run_name} busy={busy} onSend={sendWord} onAuto={autoCodeWord} />
    <div className="game-chat" aria-label="Message the desk">
      <div className="game-chat-head"><strong>Message the desk</strong></div>
      <div className="game-chat-log" ref={chatScroll.ref} onScroll={chatScroll.onScroll}>
        {chat.length === 0 && <div className="game-chat-empty">{channel ? 'No messages yet.' : 'Desk channel unavailable.'}</div>}
        {chat.map(m => <div key={m.id} className={`game-chat-item ${m.role === 'owner' ? 'owner' : 'agent'}`}><span className="game-chat-role">{m.role === 'owner' ? 'You' : 'Desk'}</span><span className="game-chat-body">{m.body}</span></div>)}
      </div>
      {chatScroll.hasNew && <button className="chat-new-messages" onClick={chatScroll.jump}>New messages · Jump to latest</button>}
      {chatOutcome && <p role="status">{chatOutcome}</p>}
        {chatError && <div className="game-error" role="alert">{chatError}</div>}
      <div className="game-chat-input">
        <textarea value={draft} onChange={e => setDraft(e.target.value)} placeholder="e.g. I am on Sokal Keep first floor, quicksave" rows={2} onKeyDown={e => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); sendChat(); } }} aria-label="Message the desk" />
        <div className="game-chat-actions">
          <button className="primary send" onClick={() => sendChat()} disabled={chatBusy || !draft.trim() || !channel}>{chatBusy ? 'Sending…' : 'Send'}</button>
        </div>
      </div>
    </div>
    </div>
  </>;
  return embedded ? content : <DashShell standalone={standalone} view="play">{content}</DashShell>;
}

function PlayWorkspace({ standalone }) {
  const [mode, setMode] = useState(() => new URLSearchParams(window.location.search).get('panel') === 'editor' ? 'editor' : 'game');
  function choose(next) {
    setMode(next);
    const url = new URL(window.location.href);
    if (next === 'editor') url.searchParams.set('panel', 'editor'); else url.searchParams.delete('panel');
    window.history.replaceState(null, '', url);
  }
  return <DashShell standalone={standalone} view="play">
    <nav className="play-subviews" aria-label="Play views">
      <button aria-pressed={mode === 'game'} onClick={() => choose('game')}>Game</button>
      <button aria-pressed={mode === 'editor'} onClick={() => choose('editor')}>Character editor</button>
    </nav>
    <div className="play-subview-body">{mode === 'editor' ? <CharacterEditor api={api} /> : <PoolradPlay embedded />}</div>
  </DashShell>;
}
export default PlayWorkspace;
