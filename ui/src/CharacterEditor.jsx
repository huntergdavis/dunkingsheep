import React, { useEffect, useState } from 'react';
import { profileKey, profilePlan, toggleProfile } from './character-editor-model.js';
import './character-editor.css';

const stats = [['strength', 'Strength'], ['intelligence', 'Intelligence'], ['wisdom', 'Wisdom'],
  ['dexterity', 'Dexterity'], ['constitution', 'Constitution'], ['charisma', 'Charisma']];
const profileLabel = p => `${p.field === 'level' ? 'Fighter level' : 'Strength'} ${p.expected_before} → ${p.value}`;

export default function CharacterEditor({ api }) {
  const [sources, setSources] = useState([]);
  const [available, setAvailable] = useState(null);
  const [files, setFiles] = useState([]);
  const [current, setCurrent] = useState(null);
  const [selected, setSelected] = useState(new Set());
  const [copyName, setCopyName] = useState('');
  const [result, setResult] = useState(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const characters = current?.characters || [];
  const plan = profilePlan(characters, selected);
  async function refresh() {
    const data = await api('/character-editor/sources');
    setSources(data.sources); setAvailable(data.available);
  }
  useEffect(() => { refresh().catch(e => { setError(e.message); setAvailable(false); }); }, []);
  function show(data) {
    setCurrent(data); setSelected(new Set()); setCopyName(`${data.name.slice(0, 58)} copy`); setResult(null);
  }
  async function inspect(id) {
    if (!id) { setCurrent(null); setResult(null); return; }
    setBusy(true); setError(''); setCurrent(null); setResult(null);
    try { show(await api(`/character-editor/sources/${id}`)); }
    catch (e) { setError(e.message); }
    finally { setBusy(false); }
  }
  async function importFiles(event) {
    event.preventDefault(); setBusy(true); setError(''); setResult(null);
    const form = new FormData(); files.forEach(file => form.append('files', file));
    try {
      show(await api('/character-editor/imports', { method: 'POST', body: form }));
      await refresh();
    } catch (e) { setError(e.message); }
    finally { setBusy(false); }
  }
  async function writeCopy(event) {
    event.preventDefault(); setBusy(true); setError(''); setResult(null);
    try {
      const data = await api('/character-editor/copies', { method: 'POST',
        headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({
          source_id: current.id, copy_name: copyName, expected_source_hashes: current.expected_source_hashes,
          profiles: plan.choices,
        }) });
      setResult(data); await refresh();
    } catch (e) { setError(e.message); }
    finally { setBusy(false); }
  }
  return <section className="character-editor" aria-labelledby="editor-title">
    <header><h1 id="editor-title">Character editor</h1>
      <p>Inspect copied DOS save files and write a new copy.</p></header>
    {error && <p className="editor-notice" role="alert">{error}</p>}
    {available === false && <p className="editor-notice">The save editor backend is unavailable.</p>}
    <div className="editor-import-row">
      <form className="editor-panel" onSubmit={importFiles}>
        <h2>Import save files</h2>
        <label htmlFor="editor-files">Select character, sidecar and world save files</label>
        <input id="editor-files" type="file" multiple accept=".sav,.itm,.spc,.dat" disabled={busy || !available}
          onChange={e => setFiles(Array.from(e.target.files))} />
        <p className="editor-help">CHRDAT*.SAV, .ITM, .SPC and SAVGAM*.DAT · up to 8 MiB total.</p>
        <button type="submit" disabled={busy || !available || !files.length}>{busy ? 'Working…' : 'Import and inspect'}</button>
      </form>
      <div className="editor-panel">
        <h2>Saved copies</h2>
        <label htmlFor="editor-source">Choose a copy to inspect</label>
        <select id="editor-source" value={current?.id || ''} disabled={busy || !available}
          onChange={e => inspect(e.target.value)}>
          <option value="">{sources.length ? 'Choose a copy…' : 'No imported copies yet'}</option>
          {sources.map(source => <option key={source.id} value={source.id}>{source.name}</option>)}
        </select>
        <button disabled={busy} onClick={() => refresh().catch(e => setError(e.message))}>Refresh copies</button>
      </div>
    </div>
    {current && <>
      <div className="editor-section-heading"><h2>{current.name}</h2>
        <span>{Object.keys(current.expected_source_hashes).length} save files</span></div>
      <p className="editor-notice">Only matching reference edits are available. Level edits do not recalculate XP, HP or spells.</p>
      <div className="editor-characters">{characters.map(character => <article className="editor-panel" key={character.file}>
        <h3>{character.file}</h3>
        <dl className="editor-stats">{stats.map(([key, label]) => <div key={key}><dt>{label}</dt><dd>{character.current_values[key]}</dd></div>)}</dl>
        <p>Reference fighter level: {character.reference_fighter_level ?? 'Not mapped'}</p>
        <fieldset disabled={busy}><legend>Supported edits</legend>
          {character.profiles.map(profile => {
            const key = profileKey(character.file, profile.validation_profile);
            const enabled = plan.available.has(key);
            const predecessor = character.profiles.find(p => p.eligible && p.eligible_after_application.includes(profile.validation_profile));
            return <label className="editor-profile" key={key}>
              <input type="checkbox" checked={selected.has(key)} disabled={!enabled}
                onChange={() => setSelected(old => toggleProfile(characters, old, key))} />
              <span>{profileLabel(profile)}{!enabled && <small>{predecessor ? `Select ${profileLabel(predecessor)} first` : 'No matching reference profile'}</small>}</span>
            </label>;
          })}
        </fieldset>
        <details><summary>Reference limits</summary><p>{character.unsupported_fields}</p>
          {character.profiles.map(profile => <p key={profile.validation_profile}>{profile.proof_scope}</p>)}
        </details>
      </article>)}</div>
      <form className="editor-panel editor-copy-form" onSubmit={writeCopy}>
        <h2>Write a new copy</h2>
        <label htmlFor="editor-copy-name">Copy name</label>
        <input id="editor-copy-name" value={copyName} maxLength={64} disabled={busy}
          onChange={e => setCopyName(e.target.value)} required />
        <p>{plan.choices.length ? `${plan.choices.length} reference edits selected.` : 'No edits selected. The exported save files will be byte identical.'} Original imported files stay unchanged.</p>
        <button type="submit" disabled={busy || !/^[A-Za-z0-9][A-Za-z0-9 _-]{0,63}$/.test(copyName)}>
          {busy ? 'Writing…' : plan.choices.length ? 'Write edited copy' : 'Write unchanged copy'}
        </button>
      </form>
    </>}
    {result && <section className="editor-panel editor-result" aria-labelledby="editor-result-title">
      <h2 id="editor-result-title">{result.name} saved</h2>
      <p role="status">Game validation: <strong>UNVERIFIED</strong> · this copy has not been loaded in the game.</p>
      <p>{result.files_written} files written · {result.byte_identical_files} byte identical</p>
      <p>Saved folder: <code>{result.output_folder}</code></p>
      <div className="editor-actions"><a className="editor-download" href={result.download_url}>Download save copy</a>
        <button onClick={() => inspect(result.id)} disabled={busy}>Inspect this copy</button></div>
      <details><summary>Write receipt · {result.receipt.files.length} files</summary>
        <ul className="editor-receipts">{result.receipt.files.map(file => <li key={file.file}>
          <strong>{file.file}</strong><span>{file.byte_identical ? 'Byte identical' : `Changed offsets: ${file.changed_offsets.map(n => `0x${n.toString(16)}`).join(', ')}`}</span>
          <dl><dt>Before SHA-256</dt><dd><code>{file.before_sha256}</code></dd>
            <dt>After SHA-256</dt><dd><code>{file.after_sha256}</code></dd></dl>
          {file.before_after_bytes.length > 0 && <p>Bytes: {file.before_after_bytes.map(b => `0x${b.offset.toString(16)}: ${b.before} → ${b.after}`).join('; ')}</p>}
        </li>)}</ul>
      </details>
    </section>}
  </section>;
}
