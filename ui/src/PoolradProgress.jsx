import React, { useEffect, useState } from 'react';
import { api } from './DeskShared';

function usePoolradProgress() {
  const [data, setData] = useState(null);
  const [error, setError] = useState('');
  async function load() {
    try { setData(await api('/poolrad-dashboard/progress')); setError(''); }
    catch (e) { setError(e.message); }
  }
  useEffect(() => { load(); const timer = setInterval(load, 60000); return () => clearInterval(timer); }, []);
  return { data, error, load };
}

function PoolradReference({ data, error }) {
  return <section id="poolrad-details" className="ops-table">
    <h2>Poolrad evidence</h2>
    {error && <p className="dash-error">{error}</p>}
    {data && <>
      <p>{data.evidence_line}</p>
      <small>Source {data.source} · status {data.status} · checkpoint {data.commit} · observed {data.observed_at} · generated {data.generated}</small>
      <p>{data.accounting_source}</p>
      <p>Replacements: {data.replacements?.count ?? 'unknown'} · {data.replacements?.state || 'unavailable'} · {data.replacements?.source} · commit {data.replacements?.commit} · observed {data.replacements?.observed_at}. Recorded in source; not rerun by dashboard.</p>
      <ul>{(data.comparisons || []).map((comparison, i) => <li key={i}>
        <strong>{comparison.label}</strong>: {comparison.matched ?? 'unknown'}/{comparison.compared ?? 'unknown'} pixels · {comparison.scope}
        <br />{comparison.status} · {comparison.method}
        <br /><a href={`/api/poolrad-dashboard/state/reports/${encodeURIComponent(comparison.source.split('/').pop())}`}>{comparison.source}</a>
      </li>)}</ul>
    </>}
  </section>;
}

export { usePoolradProgress, PoolradReference };
