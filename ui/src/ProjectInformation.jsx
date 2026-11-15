import React, { useEffect, useState } from 'react';
import { CLASS_LABELS, provenance, requestedStore, splitEntries, standing, storeSearch, viewStateText } from './project-info.js';

// t3250: read-only browser of each project's information store and the global store. Every
// value is rendered as text by React; nothing here writes to a store.
function Entry({ entry }) {
  const marks = standing(entry);
  return <article className="info-entry" data-info-entry={entry.slug}>
    <h4>{entry.subject || entry.slug} <small>{CLASS_LABELS[entry.class] || entry.class || 'Unclassified'}</small></h4>
    {provenance(entry) && <p className="info-provenance">{provenance(entry)}</p>}
    {marks.length > 0 && <p className="info-standing" role="note">{marks.join(' · ')}</p>}
    <details><summary>Show text</summary><pre className="info-body">{entry.body}{entry.body_truncated ? '\n… (truncated)' : ''}</pre></details>
  </article>;
}

// t5241: the Databases tab. The chosen store is in the address (?project=<name> or
// ?project=global), so a project's Information link opens it and a reload keeps it.
export default function ProjectInformation({ api }) {
  const [list, setList] = useState(null);
  const [listError, setListError] = useState('');
  const [selected, setSelectedState] = useState(() => requestedStore(window.location.search));
  const setSelected = value => {
    setSelectedState(value);
    const url = `${window.location.pathname}${storeSearch(value)}${window.location.hash}`;
    window.history.replaceState(window.history.state, '', url);
  };
  const [query, setQuery] = useState('');
  const [view, setView] = useState(null);
  useEffect(() => {
    api('/project-context').then(setList).catch(e => setListError(e.message));
  }, []);
  useEffect(() => {
    if (!selected) { setView(null); return undefined; }
    let live = true;
    const path = selected === 'global'
      ? '/project-context/global'
      : `/project-context/project/${encodeURIComponent(selected.slice('project:'.length))}`;
    const timer = setTimeout(() => {
      const q = query.trim() ? `?q=${encodeURIComponent(query.trim())}` : '';
      setView(null);
      api(path + q)
        .then(data => { if (live) setView(data); })
        .catch(e => { if (live) setView({ state: 'unavailable', reason: e.message }); });
    }, 250);
    return () => { live = false; clearTimeout(timer); };
  }, [selected, query]);
  const split = splitEntries(view?.entries);
  return <section className="project-group project-information" aria-label="Project information">
    <h2>Information</h2>
    {listError && <p role="alert">Information is unavailable: {listError}</p>}
    {list?.map?.state === 'invalid' && <p role="alert">The project map cannot be used: {list.map.reason}</p>}
    {list?.map?.state === 'absent' && <p className="dash-muted">No project map is configured; only the global store can be shown.</p>}
    {list && <div className="info-controls">
      <label>Store <select aria-label="Information store" value={selected} onChange={e => setSelected(e.target.value)}>
        <option value="">Choose a project</option>
        <option value="global">Global (all projects)</option>
        {(list.projects || []).map(project => <option key={project.name} value={`project:${project.name}`}>
          {project.name}{project.state === 'ok' ? '' : ' (mapping invalid)'}
        </option>)}
      </select></label>
      <label>Search <input aria-label="Search information" type="search" value={query} onChange={e => setQuery(e.target.value)} disabled={!selected} /></label>
    </div>}
    {selected && <>
      {viewStateText(view) && <p role="status">{viewStateText(view)}</p>}
      {view?.state === 'ok' && <>
        <p className="dash-muted">{view.total} entries{view.truncated ? `, first ${view.shown} shown` : ''}{view.skipped_links ? ` · ${view.skipped_links} linked file(s) skipped` : ''}</p>
        <h3>Ground truths <small>({split.groundTruths.length})</small></h3>
        {split.groundTruths.length === 0 ? <p className="dash-muted">No ground truths.</p> : split.groundTruths.map(entry => <Entry key={entry.slug} entry={entry} />)}
        <h3>Facts and history <small>({split.others.length})</small></h3>
        {split.others.map(entry => <Entry key={entry.slug} entry={entry} />)}
      </>}
    </>}
  </section>;
}
