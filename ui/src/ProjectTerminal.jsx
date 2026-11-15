import React, { useEffect, useRef, useState } from 'react';
import { Terminal } from '@xterm/xterm';
import '@xterm/xterm/css/xterm.css';
import { fencedResize } from './resize-fence.js';
import { fitFont, sizeMessage, wantedSize, FONT_MAX, FONT_MIN } from './terminal-fit.js';

// The stream is live output after one snapshot of the current screen. Never buffer or replay input.
//
// Display (t5007): the terminal always has the pane's own columns and rows, which the server
// sends as a text frame before each screen. The font is fitted to the box; the terminal is never
// given a size of its own, because a size that differs from the pane's wraps and jumps.
export default function ProjectTerminal({ target, onClose }) {
  const host = useRef(null);
  const [status, setStatus] = useState('Connecting…');
  const [note, setNote] = useState('');
  useEffect(() => {
    const term = new Terminal({ cursorBlink: true, scrollback: 5000, convertEol: false, fontSize: FONT_MAX });
    term.open(host.current);
    host.current.scrollIntoView({ block: 'nearest' });
    let socket, timer, fitFrame, resizeTimer, readyTimer, frame, disposed = false, attempt = 0;
    let paneSize = null, resizing = false, resizeReady = false, askedFor = '';
    const resizeKey = `ds-terminal-resize-unconfirmed:${target.pane_id}`;
    let resizeDisabled = true;
    try { resizeDisabled = localStorage.getItem(resizeKey) !== null; } catch { /* fail closed */ }
    const sizeProblem = 'Terminal size could not be changed';
    if (resizeDisabled) setNote(sizeProblem);
    const path = `/api/terminals/${encodeURIComponent(target.name)}`;

    // The box the terminal may fill, and the size of one cell per pixel of font size. The cell
    // is measured from what xterm rendered; before the first render a typical monospace cell.
    const box = () => {
      const style = getComputedStyle(host.current);
      const pad = name => parseFloat(style[name]) || 0;
      return { width: host.current.clientWidth - pad('paddingLeft') - pad('paddingRight'),
        height: host.current.clientHeight - pad('paddingTop') - pad('paddingBottom') };
    };
    const cell = () => {
      const screen = host.current.querySelector('.xterm-screen');
      const font = term.options.fontSize;
      const width = screen ? screen.clientWidth / term.cols / font : 0;
      const height = screen ? screen.clientHeight / term.rows / font : 0;
      return { cellWidth: width > 0 ? width : 0.6, cellHeight: height > 0 ? height : 1.2 };
    };
    // Give the terminal the pane's size, then the largest font at which it fits the box. One
    // more look after the browser has laid it out: if it still overflows, step the font down.
    const applyFit = () => {
      if (disposed || !paneSize) return;
      if (term.cols !== paneSize.cols || term.rows !== paneSize.rows) term.resize(paneSize.cols, paneSize.rows);
      const { fontSize } = fitFont({ ...box(), ...paneSize, ...cell() });
      if (term.options.fontSize !== fontSize) term.options.fontSize = fontSize;
      cancelAnimationFrame(frame);
      let steps = 0;
      const settle = () => {
        if (disposed) return;
        const screen = host.current.querySelector('.xterm-screen');
        const { width, height } = box();
        const over = screen && (screen.clientWidth > width + 1 || screen.clientHeight > height + 1);
        if (over && term.options.fontSize > FONT_MIN && steps++ < 20) {
          term.options.fontSize = Math.max(FONT_MIN, term.options.fontSize - 0.5);
          frame = requestAnimationFrame(settle);
        }
      };
      frame = requestAnimationFrame(settle);
    };

    // Ask the server to give the pane the size that fills the box. The server allows it only
    // for a pane this stream alone is attached to; a refusal is normal (the pane is shared) and
    // changes nothing here, because the display follows the pane either way.
    const resize = async () => {
      if (disposed || !resizeReady || resizeDisabled || resizing || !paneSize || socket?.readyState !== WebSocket.OPEN) return;
      const wanted = wantedSize({ ...box(), ...cell() });
      if (!wanted || (wanted.cols === paneSize.cols && wanted.rows === paneSize.rows)) return;
      const asking = `${wanted.cols}x${wanted.rows}`;
      if (asking === askedFor) return;
      askedFor = asking;
      resizing = true;
      try {
        const outcome = await fencedResize({ locks: navigator.locks, storage: localStorage, key: resizeKey,
          send: () => fetch(`${path}/resize`, { method: 'POST', credentials: 'same-origin',
            headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ cols: wanted.cols, rows: wanted.rows, expected_pane: target.pane_id }) }) });
        if (outcome === 'unconfirmed') {
          resizeDisabled = true;
          if (!disposed) setNote(sizeProblem);
        } else if (!disposed) setNote('');
      } catch {
        // A timeout may have applied the resize. Do not retry an unknown outcome.
        resizeDisabled = true;
        if (!disposed) setNote(sizeProblem);
      } finally {
        resizing = false;
      }
    };
    const boxChanged = () => {
      // Coalesce display-only font fitting with the next browser frame.
      cancelAnimationFrame(fitFrame); fitFrame = requestAnimationFrame(applyFit);
      clearTimeout(resizeTimer); resizeTimer = setTimeout(resize, 600);
    };
    const observer = new ResizeObserver(boxChanged); observer.observe(host.current);
    const connect = () => {
      if (disposed) return;
      setStatus(attempt ? 'Reconnecting…' : 'Connecting…');
      const url = new URL(`${path}/stream`, window.location.href);
      url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
      url.searchParams.set('expected_pane', target.pane_id);
      socket = new WebSocket(url); socket.binaryType = 'arraybuffer';
      socket.onopen = () => { resizeReady = false; setStatus('Connected'); term.focus(); };
      socket.onmessage = event => {
        if (disposed) return;
        // A socket handshake alone does not prove that the terminal recovered.
        if (typeof event.data !== 'string') { attempt = 0; term.write(new Uint8Array(event.data)); return; }
        // Text frames are control messages, never screen text.
        const size = sizeMessage(event.data);
        if (!size) return;
        const first = !paneSize;
        paneSize = size; askedFor = '';
        applyFit();
        // Let the first screen settle before asking for a size; its own timer, so a box change cannot cancel it.
        if (first || !resizeReady) { clearTimeout(readyTimer); readyTimer = setTimeout(() => { resizeReady = true; resize(); }, 1500); }
      };
      socket.onclose = () => {
        if (disposed) return;
        resizeReady = false; clearTimeout(readyTimer);
        setStatus('Reconnecting…');
        // Recover a working stream promptly; repeated failures still back off.
        const retry = attempt++;
        timer = setTimeout(connect, retry === 0 ? 0 : Math.min(1000 * (2 ** Math.min(retry - 1, 4)), 15000));
      };
      socket.onerror = () => socket.close();
    };
    const input = term.onData(data => {
      if (socket?.readyState !== WebSocket.OPEN) return;
      const bytes = new TextEncoder().encode(data);
      for (let offset = 0; offset < bytes.length; offset += 4096) socket.send(bytes.slice(offset, offset + 4096));
    });
    connect();
    return () => { disposed = true; clearTimeout(timer); cancelAnimationFrame(fitFrame); clearTimeout(resizeTimer); clearTimeout(readyTimer); cancelAnimationFrame(frame); observer.disconnect(); input.dispose(); socket?.close(); term.dispose(); };
  }, [target.name, target.pane_id]);
  return <section className="project-terminal" aria-label={`Terminal ${target.label || target.name}`}>
    <div className="project-terminal-header"><strong>{target.label || target.name}</strong><span role="status">{status}</span>{note && <span className="project-terminal-note" role="status">{note}</span>}<button onClick={onClose}>Close terminal</button></div>
    <div className="project-terminal-screen" ref={host} />
  </section>;
}
