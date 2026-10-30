// All projections here are scenarios. No observed lane-hours or metered C spend exist.
export function coverageEconomics(snapshot, history, { lanes = 6, creditsPerWeek = 10, now = new Date() } = {}) {
  const progress = snapshot?.function_progress;
  const events = (history?.goal_observations || [])
    .filter(row => row.goal_key === 'strict_fullc' && Number.isFinite(Number(row.value)) && Number.isFinite(Date.parse(row.observed_at)))
    .map(row => ({ at: row.observed_at, value: Number(row.value), evidence: row.evidence_ref }))
    .sort((a, b) => Date.parse(a.at) - Date.parse(b.at));
  const current = progress?.strict_full_c ?? null;
  const ceiling = progress?.ceiling ?? null;
  const remaining = Number.isFinite(current) && Number.isFinite(ceiling) ? Math.max(0, ceiling - current) : null;
  const fixedHourly = snapshot?.fixed_subscription_usd_per_calendar_hour ?? null;
  const validLanes = Number.isInteger(lanes) && lanes >= 1 && lanes <= 20;
  const validRate = Number.isFinite(creditsPerWeek) && creditsPerWeek > 0 && creditsPerWeek <= 500;
  let lastCreditAt = null;
  for (let index = 1; index < events.length; index++) {
    if (events[index].value > events[index - 1].value) lastCreditAt = events[index].at;
  }
  const nowMs = new Date(now).getTime();
  const accountingAt = Date.parse(progress?.observed_at ?? '');
  const accountingAgeMs = nowMs - accountingAt;
  const accountingFresh = Number.isFinite(accountingAgeMs) &&
    accountingAgeMs >= 0 && accountingAgeMs <= 3600 * 1000;
  const cutoff = nowMs - 24 * 3600 * 1000;
  const baseline = [...events].reverse().find(row => Date.parse(row.at) <= cutoff);
  const credits24h = accountingFresh && baseline && current != null &&
    events.at(-1)?.value === current && current >= baseline.value
    ? current - baseline.value : null;
  const scenarioWeeks = remaining != null && validRate ? remaining / creditsPerWeek : null;
  return {
    events, current, ceiling, remaining, lastCreditAt,
    credits24h, accountingAgeMinutes: Number.isFinite(accountingAgeMs) && accountingAgeMs >= 0
      ? Math.floor(accountingAgeMs / 60000) : null,
    accountingFresh,
    fixedHourly, laneHourly: fixedHourly != null && validLanes ? fixedHourly / lanes : null,
    scenarioWeeks, scenarioCalendarCost: scenarioWeeks != null && fixedHourly != null ? scenarioWeeks * 168 * fixedHourly : null,
    actualCSpendHourly: null,
  };
}

export function milestoneScenarios(current, ceiling, creditsPerWeek) {
  if (!Number.isFinite(current) || !Number.isFinite(ceiling) || !Number.isFinite(creditsPerWeek) || creditsPerWeek <= 0) return [];
  return [0.25, 0.5, 1].map(share => {
    const target = Math.ceil(ceiling * share);
    return { share, target, additional: Math.max(0, target - current), weeks: Math.max(0, target - current) / creditsPerWeek };
  });
}
