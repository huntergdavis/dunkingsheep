import React, { useEffect, useRef, useState } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';
import { fencedResize } from './resize-fence.js';

// The stream is live output, not a history snapshot. Never buffer or replay input.
export default function ProjectTerminal({ target, onClose }) {
  const host = useRef(null);
  const [status, setStatus] = useState('Connecting…');
  const [resizeWarning, setResizeWarning] = useState('');
  useEffect(() => {
    const term = new Terminal({ cursorBlink: true, scrollback: 5000, convertEol: false });
    const fit = new FitAddon(); term.loadAddon(fit); term.open(host.current);
    host.current.scrollIntoView({ block: 'nearest' });
    let socket, timer, resizeTimer, disposed = false, attempt = 0, resizing = false, resizePending = false, resizeReady = false;
    const resizeKey = `ds-terminal-resize-unconfirmed:${target.pane_id}`;
    let resizeDisabled = true;
    try { resizeDisabled = localStorage.getItem(resizeKey) !== null; } catch { /* fail closed */ }
    if (resizeDisabled) setResizeWarning('Resize is unconfirmed. Reconcile the prior resize before retrying.');
    const path = `/api/terminals/${encodeURIComponent(target.name)}`;
    const resize = async () => {
      if (disposed || !resizeReady || resizeDisabled || socket?.readyState !== WebSocket.OPEN) return;
      if (resizing) { resizePending = true; return; }
      fit.fit();
      resizing = true;
      try {
        const outcome = await fencedResize({ locks: navigator.locks, storage: localStorage, key: resizeKey,
          send: () => fetch(`${path}/resize`, { method: 'POST', credentials: 'same-origin',
            headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ cols: term.cols, rows: term.rows, expected_pane: target.pane_id }) }) });
        if (outcome === 'unconfirmed') {
          resizeDisabled = true;
          if (!disposed) setResizeWarning('Resize is unconfirmed. Reconcile the prior resize before retrying.');
        } else if (outcome === 'busy') {
          resizePending = false;
          if (!disposed) setResizeWarning('Another tab is resizing this pane.');
        } else if (outcome === 'refused') {
          resizePending = false;
          if (!disposed) setResizeWarning('Resize was refused. Reopen after the cause is resolved.');
        } else if (!disposed) setResizeWarning('');
      } catch {
        // A timeout may have applied the resize. Do not retry an unknown outcome.
        resizeDisabled = true;
        if (!disposed) setResizeWarning('Resize is unconfirmed. Reconcile the prior resize before retrying.');
      } finally {
        resizing = false;
        if (resizePending && !resizeDisabled) { resizePending = false; scheduleResize(); }
      }
    };
    const scheduleResize = () => { clearTimeout(resizeTimer); resizeTimer = setTimeout(resize, 500); };
    const observer = new ResizeObserver(scheduleResize); observer.observe(host.current);
    const connect = () => {
      if (disposed) return;
      setStatus(attempt ? 'Reconnecting…' : 'Connecting…');
      const url = new URL(`${path}/stream`, window.location.href);
      url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
      url.searchParams.set('expected_pane', target.pane_id);
      socket = new WebSocket(url); socket.binaryType = 'arraybuffer';
      socket.onopen = () => { attempt = 0; resizeReady = false; setStatus('Connected'); term.focus(); resizeTimer = setTimeout(() => { resizeReady = true; resize(); }, 1500); };
      socket.onmessage = event => { if (!disposed) term.write(typeof event.data === 'string' ? event.data : new Uint8Array(event.data)); };
      socket.onclose = () => {
        if (disposed) return;
        resizeReady = false;
        setStatus('Disconnected. Reconnecting…');
        timer = setTimeout(connect, Math.min(1000 * (2 ** Math.min(attempt++, 4)), 15000));
      };
      socket.onerror = () => socket.close();
    };
    const input = term.onData(data => {
      if (socket?.readyState !== WebSocket.OPEN) return;
      const bytes = new TextEncoder().encode(data);
      for (let offset = 0; offset < bytes.length; offset += 4096) socket.send(bytes.slice(offset, offset + 4096));
    });
    connect();
    return () => { disposed = true; clearTimeout(timer); clearTimeout(resizeTimer); observer.disconnect(); input.dispose(); socket?.close(); term.dispose(); };
  }, [target.name, target.pane_id]);
  return <section className="project-terminal" aria-label={`Terminal ${target.label || target.name}`}>
    <div className="project-terminal-header"><strong>{target.label || target.name}</strong><span role="status">{status}</span><button onClick={onClose}>Close terminal</button></div>
    <p>Live output begins when connected. For the current screen, use the existing lane view.</p>
    {resizeWarning && <p role="alert">{resizeWarning}</p>}
    <div className="project-terminal-screen" ref={host} />
  </section>;
}
