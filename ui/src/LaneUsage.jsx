import React from 'react';
import { harnessSourceText, laneUsage } from './lane-usage.js';

// t5043: one thin bar under a lane for its provider ACCOUNT's primary usage window. The label
// on every bar says it is shared by the account, so the same bar under several lanes does not
// read as per-lane usage. Estimates, stale and unavailable readings show text, not a bar.
export default function LaneUsage({ lane, budget }) {
  const usage = laneUsage(lane, budget);
  return <div className={`lane-usage lane-usage-${usage.kind}`} data-lane-usage={lane.pane_id}>
    {usage.kind === 'bar' && <div className="lane-usage-track" role="meter" aria-valuemin={0} aria-valuemax={100}
      aria-valuenow={Math.round(usage.percent)} aria-label={`${usage.account}: ${usage.text}`}>
      <div className="lane-usage-fill" style={{ width: `${usage.percent}%` }} />
    </div>}
    <small className="lane-usage-label">{usage.account} · {usage.text} · {usage.detail}</small>
    <details className="lane-usage-details">
      <summary>Usage details</summary>
      {usage.windows.length > 0 && <ul>{usage.windows.map((window, index) => <li key={index}>
        {window.name || 'window'}: {Number.isFinite(window.percent_used) ? `${Math.round(window.percent_used)}% used` : 'unknown'}
        {window.resets_at ? ` · resets ${window.resets_at}` : ''}
      </li>)}</ul>}
      {usage.extra && <p>Extra usage: {usage.extra.used_credits ?? 'unknown'} / {usage.extra.monthly_limit ?? 'unknown'} credits</p>}
      {harnessSourceText(lane) && <p className="lane-usage-source">{harnessSourceText(lane)}</p>}
      <p>These numbers belong to the {usage.account.replace(/^shared: /, '')}, shared by every lane on it. Token totals per lane are not measured yet.</p>
    </details>
  </div>;
}
