// The usage bar under each lane in the Projects view (t5043).
//
// Usage is read per provider ACCOUNT (the operations board's provider readings), never per lane:
// every lane on the same account shows the same numbers, and every bar says so. A local
// estimate (pi, opencode) is labelled as an estimate and never drawn as a limit. A provider
// that is not known, or has no reading, is "unavailable" with the reason. Nothing is invented.

function agoText(seconds) {
  if (seconds < 60) return 'less than 1 min ago';
  if (seconds < 3600) return `${Math.floor(seconds / 60)} min ago`;
  if (seconds < 48 * 3600) return `${Math.floor(seconds / 3600)} h ago`;
  return `${Math.floor(seconds / 86400)} d ago`;
}

// The age of a provider reading, in seconds, or null.
export function readingAge(provider, nowMs = Date.now()) {
  if (!provider?.measured_at) return null;
  const measured = new Date(provider.measured_at).getTime();
  if (!Number.isFinite(measured)) return null;
  return Math.max(0, Math.floor((nowMs - measured) / 1000));
}

// The provider reading a lane belongs to: its harness names the account's provider
// (opencode lanes use the 'opencode-go' account).
export function providerFor(lane, budget) {
  const harness = typeof lane?.harness === 'string' ? lane.harness.trim().toLowerCase() : '';
  if (!harness) return { reason: 'the lane\'s provider is not known' };
  const list = Array.isArray(budget?.providers) ? budget.providers : null;
  if (!list) return { name: harness, reason: 'usage readings are unavailable' };
  const provider = list.find(p => p.provider === harness
    || (typeof p.provider === 'string' && p.provider.startsWith(`${harness}-`)));
  if (!provider) return { name: harness, reason: `no usage reading for ${harness}` };
  return { name: provider.provider, provider };
}

// Everything the lane row shows about usage.
export function laneUsage(lane, budget, nowMs = Date.now()) {
  const found = providerFor(lane, budget);
  const account = found.name ? `shared: ${found.name} account` : 'shared account';
  if (!found.provider) {
    return { kind: 'unavailable', account, text: 'usage unavailable', detail: found.reason, windows: [] };
  }
  const provider = found.provider;
  const age = readingAge(provider, nowMs);
  const measured = age === null ? 'measurement time unknown' : `measured ${agoText(age)}`;
  const windows = Array.isArray(provider.windows) ? provider.windows : [];
  if (provider.meaning === 'local_session_estimate') {
    const cost = provider.local_estimate?.session_cost_usd;
    return {
      kind: 'estimate',
      account,
      text: cost == null ? 'estimate: local session cost unknown' : `estimate: local session cost $${Number(cost).toFixed(2)}`,
      detail: `${measured}; a local estimate, not an account limit`,
      windows: [],
    };
  }
  if (provider.state !== 'ok') {
    const reason = provider.reason ? `: ${provider.reason}` : '';
    const stale = provider.state === 'stale';
    return {
      kind: stale ? 'stale' : 'unavailable',
      account,
      text: stale ? 'usage reading is stale' : 'usage unavailable',
      detail: `${measured}${reason}`,
      windows,
    };
  }
  const primary = windows.find(w => Number.isFinite(w?.percent_used));
  if (!primary) {
    return { kind: 'unavailable', account, text: 'usage unavailable', detail: `${measured}: the reading has no usage window`, windows };
  }
  const percent = Math.max(0, Math.min(100, primary.percent_used));
  const resets = primary.resets_at ? `, resets ${primary.resets_at}` : '';
  return {
    kind: 'bar',
    account,
    percent,
    text: `${Math.round(percent)}% of ${primary.name || 'the window'} used`,
    detail: `${measured}${resets}`,
    windows,
    extra: provider.extra_usage || null,
  };
}
