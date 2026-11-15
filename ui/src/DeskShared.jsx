import React, { createContext, useContext, useEffect, useRef, useState } from 'react';
import { themeStyles } from './theme-styles.js';
import { buildThemeRegistry, groupThemes } from './theme-registry.js';
import { DASH_HOME, DASH_ROUTES } from './dash-route.js';
import { inboxOutcomeText, quickMessageBlock, quickMessageRoster, quickMessageTargets } from './quick-message.js';

const themeManifests = import.meta.glob('./themes/*.json', { eager: true, import: 'default' });

const THEMES = buildThemeRegistry(themeManifests, themeStyles);

const THEME_GROUPS = groupThemes(THEMES);

const ThemeContext = createContext(null);

const validTheme = value => THEMES.some(theme => theme.id === value) ? value : 'default';

function storedTheme() {
  try { return validTheme(localStorage.getItem('vp-theme')); } catch { return 'default'; }
}

const api = async (path, options = {}) => {
  const response = await fetch(`/api${path}`, { credentials: 'same-origin', ...options });
  if (!response.ok) {
    if (response.status === 401) window.dispatchEvent(new Event('vp-auth-expired'));
    const detail = await response.json().catch(() => ({}));
    const error = new Error(detail.detail || `Request failed (${response.status})`);
    error.status = response.status;
    throw error;
  }
  return response.json();
};

function notifyNote(notify) {
  const state = notify?.state;
  if (state === 'sent' || state === 'nudged') return '';
  if (state === 'queued') return ' · held by the typing guard; it arrives when the desk box is clear';
  if (state === 'unconfirmed') return ' · sent, not confirmed; the desk may already have it';
  return ` · ${notify?.detail || 'desk not reached'}`;
}

function DashHeader({ side, view, standalone }) {
  if (!side && (!view || standalone)) return null;
  return <header className="dash-header">
    <div className="dash-side">
      {side}
      {view && !standalone && <a className="dash-link" href={`/dash/${view}`} title="Open this dashboard on its own route">Standalone ↗</a>}
    </div>
  </header>;
}

function DashNav({ view, side }) {
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [messageOpen, setMessageOpen] = useState(false);
  const settingsButton = useRef(null);
  const messageButton = useRef(null);
  const boards = useRef(null);
  useEffect(() => {
    function dismiss(event) {
      if (!boards.current?.open) return;
      if (event.type === 'keydown' && event.key === 'Escape') {
        boards.current.open = false;
        boards.current.querySelector('summary').focus();
      } else if (event.type === 'pointerdown' && !boards.current.contains(event.target)) {
        boards.current.open = false;
      }
    }
    document.addEventListener('keydown', dismiss);
    document.addEventListener('pointerdown', dismiss);
    return () => {
      document.removeEventListener('keydown', dismiss);
      document.removeEventListener('pointerdown', dismiss);
    };
  }, []);
  // Ctrl+/ opened the retired 'Message the desk' dock; it now opens the general Message dialog.
  useEffect(() => {
    function shortcut(event) {
      if ((event.ctrlKey || event.metaKey) && (event.key === '/' || event.code === 'Slash')) { event.preventDefault(); setMessageOpen(true); }
    }
    window.addEventListener('keydown', shortcut);
    return () => window.removeEventListener('keydown', shortcut);
  }, []);
  const links = Object.entries(DASH_ROUTES).map(([key, route]) => <a key={key} href={`/dash/${key}`} aria-current={view === key ? 'page' : undefined}>{route.title}</a>);
  function closeSettings() { setSettingsOpen(false); settingsButton.current?.focus(); }
  return <nav className="dash-nav" aria-label="Dashboards">
    <a className="dash-nav-brand" href={DASH_HOME} aria-label="Dunking Sheep home: Projects"><span className="dash-nav-mark">DS</span></a>
    <span className="dash-nav-links">{links}</span>
    <details className="dash-nav-boards" ref={boards}>
      <summary>Boards <span aria-hidden="true">▾</span></summary>
      <div className="dash-nav-links">{links}</div>
    </details>
    {side && <span className="dash-nav-status">{side}</span>}
    <span className="dash-nav-right"><button className="dock-trigger" aria-label="Quick message" title="Message a pane" ref={messageButton} onClick={() => setMessageOpen(true)}><span className="nav-action-label">Message</span><span className="nav-action-icon" aria-hidden="true">✉</span> <kbd>Ctrl+/</kbd></button><button className="dock-trigger" aria-label="Appearance" title="Appearance" ref={settingsButton} onClick={() => setSettingsOpen(true)}><span className="nav-action-label">Appearance</span><span className="nav-action-icon" aria-hidden="true">◐</span></button></span>
    {settingsOpen && <AppearanceSettings onClose={closeSettings} />}
    {messageOpen && <QuickMessage onClose={() => { setMessageOpen(false); messageButton.current?.focus(); }} />}
  </nav>;
}

function DashShell({ standalone, view, side, children }) {
  return <div className={`dash-root ${standalone ? 'dash-standalone' : 'dash-embedded'}`}>
    {standalone && <DashNav view={view} side={side} />}
    {children}
  </div>;
}

function Login({ onSuccess }) {
  const [password, setPassword] = useState('');
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const [next] = useState(() => new URLSearchParams(window.location.search).get('next') || '');
  async function submit(event) {
    event.preventDefault(); setBusy(true); setError('');
    try {
      const result = await api('/login', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ password, next }) });
      if (result.next) { window.location.assign(result.next); return; }
      onSuccess();
    } catch (e) { setError(e.message); } finally { setBusy(false); }
  }
  return <main className="login-shell"><div className="login-card">
    <div className="brand-mark">VA</div><div className="eyebrow">PRIVATE EXECUTIVE DESK</div>
    <h1>Dunking Sheep</h1><p>Your fleet: ask the desk, watch the board, play.</p>
    <form onSubmit={submit}><label htmlFor="password">Owner password</label>
      <input id="password" type="password" value={password} onChange={e => setPassword(e.target.value)} autoFocus required />
      {error && <div className="form-error" role="alert">{error}</div>}
      <button className="primary" disabled={busy}>{busy ? 'Signing in…' : 'Open desk'}</button></form>
  </div></main>;
}

function PasswordSettings({ onClose }) {
  const [current, setCurrent] = useState(''); const [next, setNext] = useState(''); const [confirm, setConfirm] = useState('');
  const [busy, setBusy] = useState(false); const [error, setError] = useState(''); const [saved, setSaved] = useState(false);
  async function submit(event) {
    event.preventDefault(); setError('');
    if (next !== confirm) { setError('New passwords do not match'); return; }
    setBusy(true);
    try { await api('/password', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ current_password: current, new_password: next }) });
      setCurrent(''); setNext(''); setConfirm(''); setSaved(true);
    } catch (e) { setError(e.message); } finally { setBusy(false); }
  }
  return <div className="modal-backdrop" onMouseDown={onClose}><div className="settings-modal" onMouseDown={e => e.stopPropagation()} role="dialog" aria-modal="true" aria-label="Change owner password">
    <div className="settings-head"><div><div className="eyebrow">OWNER ACCESS</div><h2>Change password</h2></div><button className="icon-button" onClick={onClose} aria-label="Close">×</button></div>
    <form onSubmit={submit}><label>Current password<input type="password" autoComplete="current-password" value={current} onChange={e => setCurrent(e.target.value)} required /></label>
      <label>New password<input type="password" autoComplete="new-password" minLength={12} value={next} onChange={e => setNext(e.target.value)} required /></label>
      <label>Confirm new password<input type="password" autoComplete="new-password" minLength={12} value={confirm} onChange={e => setConfirm(e.target.value)} required /></label>
      {error && <div className="form-error" role="alert">{error}</div>}{saved && <div className="settings-success" role="status">Password changed. Other browsers were signed out.</div>}
      <button className="primary" disabled={busy}>{busy ? 'Saving…' : 'Save password'}</button></form>
  </div></div>;
}

function AppearanceSettings({ onClose }) {
  const { theme, chooseTheme, themeError } = useContext(ThemeContext);
  const modal = useRef(null);
  function keyDown(event) {
    if (event.key === 'Escape') { event.preventDefault(); onClose(); }
    if (event.key !== 'Tab') return;
    const controls = [...modal.current.querySelectorAll('button:not(:disabled), select:not(:disabled)')];
    const first = controls[0], last = controls.at(-1);
    if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
    else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
  }
  return <div className="modal-backdrop" onMouseDown={onClose}><div className="settings-modal" ref={modal} onKeyDown={keyDown} onMouseDown={e => e.stopPropagation()} role="dialog" aria-modal="true" aria-label="Appearance">
    <div className="settings-head"><h2>Appearance</h2><button className="icon-button" onClick={onClose} aria-label="Close appearance">×</button></div>
    <label htmlFor="desk-theme">Theme</label>
    <select id="desk-theme" value={theme} onChange={event => chooseTheme(event.target.value)} autoFocus>
      {THEME_GROUPS.map(group => <optgroup key={group.label} label={group.label}>
        {group.options.map(option => <option key={option.id} value={option.id}>{option.name}</option>)}
      </optgroup>)}
    </select>
    {themeError && <p role="status">Theme preference could not be saved. Using Default.</p>}
  </div></div>;
}

function QuickMessage({ onClose }) {
  const [fleet, setFleet] = useState(null);
  const [load, setLoad] = useState({ status: 'loading', error: '' });
  const [attempt, setAttempt] = useState(0);
  const [pane, setPane] = useState('');
  const [draft, setDraft] = useState('');
  const [busy, setBusy] = useState(false);
  const [sent, setSent] = useState('');
  const [sendError, setSendError] = useState('');
  const sending = useRef(false);
  useEffect(() => {
    let live = true;
    setLoad({ status: 'loading', error: '' });
    api('/fleet')
      .then(data => { if (!live) return; setFleet(data); setLoad({ status: 'ready', error: '' }); })
      .catch(e => { if (live) setLoad({ status: 'failed', error: e.message }); });
    return () => { live = false; };
  }, [attempt]);
  const roster = quickMessageRoster(load, fleet);
  const targets = quickMessageTargets(fleet);
  const projects = [...new Set(targets.map(target => target.project))];
  const block = quickMessageBlock(targets, pane, draft, busy);
  async function send(event) {
    event.preventDefault();
    if (block || sending.current) return;
    sending.current = true; setBusy(true); setSent(''); setSendError('');
    try {
      const data = await api('/pane-inbox', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ pane, text: draft }) });
      setDraft(current => current === draft ? '' : current);
      setSent(inboxOutcomeText(data, targets.find(target => target.pane_id === pane)));
    } catch (e) { setSendError(e.message); }
    finally { sending.current = false; setBusy(false); }
  }
  return <div className="modal-backdrop" onMouseDown={onClose}><div className="settings-modal quick-message" onKeyDown={event => { if (event.key === 'Escape') { event.preventDefault(); onClose(); } }} onMouseDown={e => e.stopPropagation()} role="dialog" aria-modal="true" aria-label="Quick message">
    <div className="settings-head"><div><div className="eyebrow">QUICK MESSAGE</div><h2>Leave an inbox message</h2></div><button className="icon-button" onClick={onClose} aria-label="Close quick message">×</button></div>
    {roster.kind === 'failed' && <div className="form-error" role="alert">{roster.text} <button type="button" onClick={() => setAttempt(n => n + 1)}>Retry</button></div>}
    {roster.kind === 'loading' && <p role="status">{roster.text}</p>}
    {roster.kind === 'empty' && <p role="status">{roster.text} <button type="button" onClick={() => setAttempt(n => n + 1)}>Reload</button></p>}
    <form onSubmit={send}>
      <label htmlFor="quick-message-pane">Pane</label>
      <select id="quick-message-pane" value={pane} onChange={event => { setPane(event.target.value); setSent(''); setSendError(''); }} autoFocus>
        <option value="">Choose a pane…</option>
        {projects.map(project => <optgroup key={project} label={project}>
          {targets.filter(target => target.project === project).map(target => <option key={target.pane_id} value={target.pane_id}>{target.label}</option>)}
        </optgroup>)}
      </select>
      <label htmlFor="quick-message-text">Message</label>
      <textarea id="quick-message-text" rows={4} value={draft} onChange={event => setDraft(event.target.value)} placeholder="Write a message" />
      {/* Send and Cancel sit on their own row under the text box, so Send is always in view. */}
      <div className="quick-message-actions">
        <button className="primary" type="submit" disabled={Boolean(block)} title={block || 'Send message'}>{busy ? 'Sending…' : 'Send'}</button>
        <button type="button" onClick={onClose}>Cancel</button>
      </div>
      {sent && <p className="settings-success" role="status">{sent}</p>}
      {sendError && <div className="form-error" role="alert">{sendError}</div>}
    </form>
  </div></div>;
}

export { api, DashHeader, DashShell, notifyNote, ThemeContext, validTheme, storedTheme, Login };
