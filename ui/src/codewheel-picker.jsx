import React, { useRef, useState } from 'react';
import { PATH_NAMES, RINGS, templateUrl, wheelWord } from './codewheel.js';

function RuneChoice({ ring, number, selected, onSelect }) {
  const [missing, setMissing] = useState(false);
  const name = ring === 'esp' ? 'Espruar' : 'Dethek';
  return <button type="button" className="rune-choice" aria-label={`${name} rune ${number}`} aria-pressed={selected} onClick={onSelect}>
    {missing ? <span className="rune-missing">No image</span> : <img src={templateUrl(ring, number)} alt="" loading="lazy" onError={() => setMissing(true)} />}
    <span>{number}</span>
  </button>;
}

export function CodewheelPicker({ running, runKey, busy, onSend, onAuto }) {
  const [a, setA] = useState(null), [b, setB] = useState(null), [path, setPath] = useState(null);
  const [open, setOpen] = useState(false), [analyzing, setAnalyzing] = useState(false), [message, setMessage] = useState('');
  const inFlight = useRef(false);
  const word = wheelWord(a, b, path), locked = busy || analyzing;

  async function auto() {
    if (inFlight.current || busy || !running) return;
    inFlight.current = true; setAnalyzing(true); setMessage('Reading the runes on the server…');
    try {
      const result = await onAuto();
      if (!result?.ok) { setOpen(true); setMessage(`No typing: ${result?.error || 'detection failed'}. Choose both runes and the path manually, or enter a known word above.`); return; }
      if (result.typed && result.word) {
        setA(result.a); setB(result.b); setPath(result.path);
        setMessage(`Typed ${result.word} + Enter.`);
        return;
      }
      setOpen(true);
      setMessage(`No typing: ${result.reason || 'the runes did not match clearly enough.'}${result.path !== null && result.path !== undefined ? ` Visible path: ${PATH_NAMES[result.path]}.` : ''} Choose both runes and the path manually, or enter a known word above.`);
    } catch (error) {
      setOpen(true); setMessage(`No typing: ${error.message}`);
    } finally { inFlight.current = false; setAnalyzing(false); }
  }

  async function manual() {
    if (!word || !running || busy || inFlight.current) return;
    inFlight.current = true;
    try { const ok = await onSend(word); setMessage(ok ? `Typed ${word} + Enter.` : 'Delivery was not confirmed. Inspect the game before sending again.'); }
    finally { inFlight.current = false; }
  }
  const numberValue = value => value === '' ? null : Number(value);
  return <section className="codewheel-panel" aria-label="Code word helper">
    <div className="game-wheel">
      <strong>Code wheel</strong>
      <button onClick={auto} disabled={locked || !running}>{analyzing ? 'Reading runes…' : 'Auto code word'}</button>
      <label>Espruar <input type="number" min="1" max="36" value={a ?? ''} onChange={e => setA(numberValue(e.target.value))} aria-label="Espruar rune number" disabled={locked} /></label>
      <label>Dethek <input type="number" min="1" max="36" value={b ?? ''} onChange={e => setB(numberValue(e.target.value))} aria-label="Dethek rune number" disabled={locked} /></label>
      <select value={path ?? ''} onChange={e => setPath(numberValue(e.target.value))} aria-label="Wheel path" disabled={locked}>
        <option value="">Choose path…</option>{PATH_NAMES.map((name, i) => <option key={i} value={i}>{name}</option>)}
      </select>
      <output className="game-wheel-word" aria-label="Computed code word">{word || '—'}</output>
      <button onClick={manual} disabled={locked || !running || !word}>Type code word</button>
    </div>
    <p className="wheel-help">For the DOS Decimer screen, use the manual wheel or known-word entry above. Auto only types when both wheel runes and the path match clearly.</p>
    {message && <p className="wheel-result" role="status">{message}</p>}
    <details className="wheel-picker" open={open} onToggle={event => setOpen(event.currentTarget.open)}>
      <summary>Visual rune picker</summary>
      <div className="wheel-rings">{RINGS.map(ring => <fieldset key={ring} disabled={locked}>
        <legend>{ring === 'esp' ? 'Espruar · outer ring' : 'Dethek · inner ring'}</legend>
        <div className="rune-grid">{Array.from({ length: 36 }, (_, i) => <RuneChoice key={i} ring={ring} number={i + 1} selected={(ring === 'esp' ? a : b) === i + 1} onSelect={() => (ring === 'esp' ? setA : setB)(i + 1)} />)}</div>
      </fieldset>)}</div>
    </details>
  </section>;
}
