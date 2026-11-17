import React from 'react';
const percent = value => Number.isFinite(value) ? `${value.toFixed(1)}%` : 'Unknown';
export default function ResourceSummary({ resources }) {
  const host = resources?.host;
  return <section className="project-resources" aria-label="Shared host health">
    <strong>Shared host</strong>
    <span>CPU {percent(host?.cpu_used_percent)}</span>
    <span>Memory {percent(host?.memory?.used_percent)}</span>
    <span>CPU pressure {percent(host?.cpu_pressure_avg60)}</span>
    <span>Memory pressure {percent(host?.memory_pressure_avg60)}</span>
    {host?.saved_health && <span>{host.saved_health.stale ? 'Stale disk sample · ' : ''}Temporary disk free {Number.isFinite(host.saved_health.tmp_free_gib) ? host.saved_health.tmp_free_gib.toFixed(1) : 'Unknown'} GiB</span>}
    <small>{resources ? 'Shared across projects · pressure averaged over 60 seconds' : 'Resource readings unavailable'}</small>
  </section>;
}
