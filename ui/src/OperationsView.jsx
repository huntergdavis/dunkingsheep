import React, { useEffect, useRef, useState } from 'react';
import { coverageEconomics, milestoneScenarios } from './coverage-economics.js';
import { leaderLine } from './leader-line.js';
import { laneStatus } from './lane-status.js';
import { laneDisplayName } from './lane-label.js';
import { sendOutcomeText } from './quick-message.js';
import { api, DashHeader, DashShell } from './DeskShared';
import { usePoolradProgress, PoolradReference } from './PoolradProgress';

function LaneStatus({ row }) {
  const value = laneStatus(row);
  return <span data-lane-status={row.pane_id}>{value.status}<small>{value.provenance}</small></span>;
}

function LineTrend({ title, points, color, unit, note, step = false }) {
  const rows = points.filter(point => Number.isFinite(point.value) && Number.isFinite(Date.parse(point.at)));
  if (!rows.length) return <div className="ops-chart"><h3>{title}</h3><p className="muted">No observations in this window</p><small>{note}</small></div>;
  const first = Date.parse(rows[0].at), last = Date.parse(rows.at(-1).at);
  const high = Math.max(1, ...rows.map(row => row.value));
  const low = Math.min(0, ...rows.map(row => row.value));
  const x = row => 24 + (last === first ? 0.5 : (Date.parse(row.at) - first) / (last - first)) * 336;
  const y = row => 112 - ((row.value - low) / (high - low || 1)) * 92;
  const latest = rows.at(-1);
  return <div className="ops-chart"><div className="ops-chart-head"><h3>{title}</h3><strong>{latest.value} {unit}</strong></div>
    <svg viewBox="0 0 384 140" role="img" aria-label={`${title}: ${rows.length} timestamped observations, latest ${latest.value} ${unit}`}>
      <path d="M24 112 H360" stroke="#58717e" fill="none" />
      {step && rows.length > 1 && <path d={rows.map((row, index) => index ? `H${x(row)} V${y(row)}` : `M${x(row)} ${y(row)}`).join(' ')} fill="none" stroke={color} strokeWidth="3" />}
      {rows.map((row, index) => <circle key={`${row.at}-${index}`} cx={x(row)} cy={y(row)} r="4" fill={color}><title>{new Date(row.at).toLocaleString()}: {row.value} {unit}</title></circle>)}
      <text x="24" y="135">{new Date(rows[0].at).toLocaleString()}</text><text x="360" y="135" textAnchor="end">{new Date(latest.at).toLocaleTimeString()}</text>
    </svg><small>{note}</small></div>;
}

function MergeTrend({ merges, start, end }) {
  const begin = Date.parse(start), finish = Date.parse(end);
  if (!Number.isFinite(begin) || !Number.isFinite(finish)) return null;
  const buckets = Array.from({ length: 12 }, () => 0);
  for (const merge of merges) {
    const time = Date.parse(merge.merged_at);
    if (time >= begin && time <= finish) buckets[Math.min(11, Math.floor((time - begin) / (finish - begin) * 12))]++;
  }
  const high = Math.max(1, ...buckets);
  return <div className="ops-chart"><div className="ops-chart-head"><h3>PR merges · 24 hours</h3><strong>{merges.length} merged</strong></div>
    <svg viewBox="0 0 384 140" role="img" aria-label={`${merges.length} tracked PR merges in twelve two-hour intervals`}>
      <path d="M24 112 H360" stroke="#58717e" fill="none" />
      {buckets.map((count, index) => <rect key={index} x={28 + index * 28} y={112 - count / high * 92} width="18" height={count / high * 92} fill="#eab46f"><title>Bucket {index + 1}: {count} merges</title></rect>)}
      <text x="24" y="135">{new Date(start).toLocaleTimeString()}</text><text x="360" y="135" textAnchor="end">Now</text>
    </svg><small>Recent visible PRs only. A merge is not a deployment or strict C credit.</small></div>;
}

function QuotaCard({ provider, quota }) {
  const name = provider === 'codex' ? 'CODEX' : 'CLAUDE';
  return <div>
    <b>{name} WEEKLY QUOTA</b>
    <strong>{quota ? quota.weekly_percent_left + '% left' : 'No reading'}</strong>
    <small>{quota
      ? `${quota.age_s > 3600 ? 'STALE · ' : ''}Observed ${Math.round(quota.age_s / 60)} min ago in ${quota.pane} · resets ${new Date(quota.reset_at).toLocaleString()} · other accounts unknown`
      : 'Awaiting a sourced quota update'}</small>
  </div>;
}

function CreditTrend({ events, current, ceiling, lastCreditAt }) {
  if (!events.length) return <div className="ops-credit-chart"><p>No verified credit events in the seven-day window.</p></div>;
  const begin = Date.parse(events[0].at), end = Date.now();
  const width = 760, left = 52, right = 738, top = 22, bottom = 177;
  const min = Math.max(0, Math.min(...events.map(row => row.value)) - 2);
  const max = Math.max(min + 1, current ?? 0, ...events.map(row => row.value));
  const x = at => left + (Date.parse(at) - begin) / Math.max(1, end - begin) * (right - left);
  const y = value => bottom - (value - min) / (max - min) * (bottom - top);
  const path = events.map((row, index) => `${index ? `H${x(row.at).toFixed(1)} V` : 'M' + x(row.at).toFixed(1) + ' '}${y(row.value).toFixed(1)}`).join(' ') + ` H${right}`;
  const recordedGain = events.at(-1).value - events[0].value;
  return <div className="ops-credit-chart">
    <div className="ops-credit-title"><div><span className="ops-kicker">VERIFIED CREDIT HISTORY</span><h3>Strict C functions · 7 days</h3></div><strong>{current ?? '—'} <small>/ {ceiling ?? '—'}</small></strong></div>
    <svg viewBox={`0 0 ${width} 215`} role="img" aria-label={`Step chart of ${events.length} verified strict C checkpoints; current ${current ?? 'unknown'} of ${ceiling ?? 'unknown'}`}>
      {[0, .5, 1].map(fraction => <g key={fraction}><path d={`M${left} ${y(min + (max - min) * fraction)} H${right}`} stroke="#35505d" strokeDasharray="3 5"/><text x="7" y={y(min + (max - min) * fraction) + 4}>{Math.round(min + (max - min) * fraction)}</text></g>)}
      <path d={path} fill="none" stroke="#a9db7d" strokeWidth="3" strokeLinejoin="round" />
      {events.map((row, index) => <circle key={`${row.at}-${index}`} cx={x(row.at)} cy={y(row.value)} r="4" fill="#d7efb8"><title>{new Date(row.at).toLocaleString()}: {row.value} reviewed functions</title></circle>)}
      <text x={left} y="204">{new Date(events[0].at).toLocaleDateString()}</text><text x={right} y="204" textAnchor="end">Now</text>
    </svg>
    <p>{recordedGain >= 0 ? '+' : ''}{recordedGain} net since the first visible checkpoint on {new Date(events[0].at).toLocaleDateString()}. Last new credit: {lastCreditAt ? new Date(lastCreditAt).toLocaleString() : 'unknown'}. The horizontal tail means no later credited change is recorded; it does not measure work in progress.</p>
  </div>;
}

function RecreationLab({ snapshot, history }) {
  const lab = snapshot.experiment_lab || {};
  const model = coverageEconomics(snapshot, history);
  const roster = Object.fromEntries((snapshot.pane_roster || []).map(pane => [pane.pane_id, pane]));
  const messages = lab.messages_by_pane || [];
  const trials = lab.trials || [];
  const efficiency = lab.efficiency_by_type_model || [];
  const closedTrials = efficiency.reduce((total, row) => total + (row.completed_trials || 0), 0);
  const acceptedInClosedTrials = efficiency.reduce((total, row) => total + (row.completed_accepted_bodies || 0), 0);
  const costedClosedTrials = trials.filter(row => row.ended_at && row.metered_usd != null);
  const sourcedTrialSpend = costedClosedTrials.length
    ? costedClosedTrials.reduce((total, row) => total + row.metered_usd, 0)
    : null;
  const costsSourced = efficiency.filter(row => row.completed_trials > 0)
    .every(row => row.metered_usd != null);
  const trialDecision = closedTrials === 0
    ? 'No completed model trials yet.'
    : acceptedInClosedTrials === 0
      ? `No model winner: ${closedTrials} closed trials, zero accepted bodies.`
      : !costsSourced
        ? `No cost winner: ${acceptedInClosedTrials} accepted bodies in closed trials, with incomplete usage costs.`
        : 'Observed accepted-body rates and costs are available below; compare completed trials before choosing a lane mix.';
  const maxMessages = Math.max(1, ...messages.map(row => row.inbox_deliveries + row.direct_sends));
  const unknown = value => value == null ? 'Unmeasured' : value;
  return <section className="ops-mission" aria-label="815 candidate C bodies and agent experiment lab">
    <div className="ops-mission-head"><div><span className="ops-kicker">PRIMARY PROJECT GOAL / ORIGINAL C BODIES</span><h2>Recreate all 815 candidate bodies</h2><p>815 is the PM-reviewed static ceiling for non-library original starts, not a proven exact game-function count. Count substantive C bodies by original address; tests, PRs and demo work are quality or activity signals.</p></div><strong>{model.current ?? '—'} <span>/ {model.ceiling ?? 815}</span></strong></div>
    <div className="ops-mission-cards">
      <div><b>PUBLISHED PM CREDIT</b><strong>{model.current ?? '—'}</strong><small>{model.remaining ?? '—'} candidate targets still uncredited. Reviewed main {snapshot.function_progress?.commit_oid?.slice(0, 8) || 'unavailable'}; accounting observed {model.accountingAgeMinutes == null ? 'at an unknown time' : `${model.accountingAgeMinutes} min ago`}{model.accountingFresh ? '' : ' (stale)'}. Ceiling: PM-reviewed census PR #819.</small></div>
      <div><b>NEW CREDIT / 24H</b><strong>{model.credits24h == null ? 'Unknown' : `+${model.credits24h}`}</strong><small>Comparable reviewed checkpoints and an accounting sample within 60 minutes. A local draft does not change this number.</small></div>
      <div><b>NEW BODY LEDGER</b><strong>{lab.authored_bodies ?? 0} <span>authored</span></strong><small>{lab.accepted_bodies ?? 0} independently accepted since tracking began; logged separately from published credit to avoid double counting.</small></div>
      <div><b>SOURCED TRIAL SPEND</b><strong>{sourcedTrialSpend == null ? 'Unmeasured' : `$${sourcedTrialSpend.toFixed(2)}`}</strong><small>{costedClosedTrials.length}/{closedTrials} closed trials have attributed usage costs. This is trial spend only; model efficiency per dollar needs accepted bodies and complete cost records.</small></div>
    </div>
    <div className="ops-mission-main"><CreditTrend events={model.events} current={model.current} ceiling={model.ceiling} lastCreditAt={model.lastCreditAt}/>
      <div className="ops-lab-panel"><span className="ops-kicker">NEXT PROOF POINT</span><h3>First newly accepted C body</h3><p>PM reviews an exact authored head for source completeness. Write its focused test after the body and run bounded parity when host capacity permits. Record author/model, elapsed time, review effort and sourced cost.</p><small>ETA: no measured accepted-body rate yet. The existing 82 credits do not establish a current completion pace.</small></div></div>
    <div className="ops-lab-grid"><div className="ops-lab-panel"><div className="ops-lab-head"><h3>Messages delivered per agent · 24h</h3><span>{snapshot.sources?.agent_messages || 'unavailable'}</span></div>
      <div className="ops-table-wrap"><table><thead><tr><th>Agent</th><th>Type</th><th>Inbox</th><th>Direct</th></tr></thead><tbody>{messages.slice(0, 20).map(row => <tr key={row.pane_id}><td title={row.pane_id}>{row.agent_name || roster[row.pane_id]?.agent_name || 'Unnamed agent'}{(row.lane_name || roster[row.pane_id]?.lane_name) && <small>{row.lane_name || roster[row.pane_id]?.lane_name}</small>}<div className="ops-message-bar"><i style={{width:`${(row.inbox_deliveries + row.direct_sends) / maxMessages * 100}%`}} /></div></td><td>{roster[row.pane_id]?.agent_kind || 'Unknown'}</td><td>{row.inbox_deliveries}</td><td>{row.direct_sends}</td></tr>)}{!messages.length && <tr><td colSpan="4">No sourced delivery events in this window.</td></tr>}</tbody></table></div>
      <small>{lab.message_definition || 'Waiting for the delivery-event source.'} Counts are stored in the private database with event IDs and timestamps.</small></div>
      <div className="ops-lab-panel"><div className="ops-lab-head"><h3>Model and reasoning trials</h3><span>{trials.length} recorded</span></div>
      <p><b>TRIAL DECISION:</b> {trialDecision}</p>
      <div className="ops-table-wrap"><table><thead><tr><th>Type / model</th><th>Effort</th><th>Bodies</th><th>Accepted/hr</th><th>$/accepted</th></tr></thead><tbody>{efficiency.map(row => <tr key={`${row.agent_type}-${row.model}-${row.reasoning_effort}`}><td>{row.agent_type}<small>{row.model} · {row.completed_trials}/{row.trials} trials closed</small></td><td>{row.reasoning_effort}</td><td>{row.authored_bodies} / {row.accepted_bodies}</td><td>{unknown(row.accepted_per_hour)}</td><td>{row.usd_per_accepted == null ? 'Unmeasured' : `$${row.usd_per_accepted.toFixed(2)}`}</td></tr>)}{!efficiency.length && <tr><td colSpan="5">No trial recorded yet. Opus/Sonnet and Codex reasoning comparisons are queued.</td></tr>}</tbody></table></div>
      <small>Body counts are distinct authored function outputs per trial / latest exact-head acceptances. Later repairs retain the earlier trial's authored output without assigning its acceptance. Rates use completed trial hours; review time and usage dollars appear only where sourced.</small>
      <section><h2>Individual trial records</h2><div className="ops-table-wrap"><table><thead><tr><th>Pane</th><th>Model / effort</th><th>Minutes</th><th>Messages</th><th>Review</th></tr></thead><tbody>{trials.map(row => <tr key={row.trial_id}><td>{row.pane_id}</td><td>{row.model}<small>{row.reasoning_effort || 'unspecified'}</small></td><td>{Math.round(row.elapsed_hours * 60)}</td><td>{row.inbox_deliveries} inbox / {row.direct_sends} direct</td><td>{row.correction_minutes == null ? 'Unmeasured' : `${row.correction_minutes} min`}</td></tr>)}</tbody></table></div></section>
      <small>Stored per trial: model, provider, reasoning effort, start/end, function IDs and status, review time, delivered messages, host PSI, tokens and measured cost when sourced.</small></div></div>
  </section>;
}

function CoverageEconomics({ snapshot, history }) {
  const [lanes, setLanes] = useState(6);
  const [weekly, setWeekly] = useState(10);
  const model = coverageEconomics(snapshot, history, { lanes, creditsPerWeek: weekly });
  const milestones = milestoneScenarios(model.current, model.ceiling, weekly);
  const laneGoal = snapshot.goals?.find(goal => goal.goal_key === 'strict_c_assigned_lanes');
  const compileGoal = snapshot.goals?.find(goal => goal.goal_key === 'strict_c_compile_slots');
  const money = value => value == null ? 'Unavailable' : '$' + value.toFixed(2);
  return <section className="ops-executive" aria-label="C coverage economics and outlook">
    <div className="ops-executive-head"><div><span className="ops-kicker">POOLRAD / EXECUTIVE OUTLOOK</span><h2>C coverage economics</h2><p>Measured credit, fixed commitment, and a changeable planning scenario.</p></div><span className="ops-evidence-pill">PM-REVIEWED CREDIT</span></div>
    <div className="ops-executive-cards">
      <div><span className="ops-kicker">CREDITED FUNCTIONS</span><strong>{model.current ?? '—'} <em>/ {model.ceiling ?? '—'}</em></strong><small>{model.remaining ?? '—'} remain to the working ceiling. This is source coverage, not playable progress.</small></div>
      <div><span className="ops-kicker">LAST 24 HOURS</span><strong>{model.credits24h == null ? 'Unknown' : `+${model.credits24h}`}</strong><small>{model.credits24h == null ? 'No comparable verified checkpoint.' : 'Net new PM-reviewed strict C functions; candidate work earns no credit.'}</small></div>
      <div><span className="ops-kicker">FIXED POOL / CALENDAR HOUR</span><strong>{model.fixedHourly == null ? 'Unknown' : money(model.fixedHourly) + '/hr'}</strong><small>{snapshot.monthly_fixed_usd == null ? 'Owner cost source unavailable.' : `${money(snapshot.monthly_fixed_usd)}/month ÷ ${snapshot.calendar_hours_in_month} calendar hours.`} Excludes metered usage.</small></div>
      <div className="ops-unavailable"><span className="ops-kicker">ACTUAL C SPEND / HOUR</span><strong>Unavailable</strong><small>No measured C lane-hours, provider billing allocation, or metered usage series. Quota percentages are not dollars.</small></div>
    </div>
    <div className="ops-capacity-line"><b>C STAFFING / EXECUTION</b><span>{laneGoal?.status === 'verified' && laneGoal.value != null ? `${laneGoal.value} assigned / ${laneGoal.target ?? 6} planned` : 'Current C assignments unverified'}{compileGoal?.value != null ? ` · ${compileGoal.value} compile/native slots authorized` : ''}{laneGoal?.observed_at ? ` · PM checkpoint ${new Date(laneGoal.observed_at).toLocaleString()}` : ''}. Assignment is not productive lane time; the six-lane cost control is a planning assumption.</span></div>
    {model.credits24h != null && model.fixedHourly != null && <div className="ops-signal-line"><b>24-HOUR SIGNAL</b><span>+{model.credits24h} new reviewed C functions while {money(model.fixedHourly * 24)} of known fixed subscriptions elapsed across all work. This is not C-attributed spend or evidence of productive lane-hours.</span></div>}
    <div className="ops-executive-body"><CreditTrend events={model.events} current={model.current} ceiling={model.ceiling} lastCreditAt={model.lastCreditAt} />
      <div className="ops-model"><span className="ops-kicker">PLANNING SCENARIO · NOT A FORECAST</span><h3>What would it take?</h3>
        <p>Assume the whole fixed subscription pool supports {lanes} C lanes every calendar hour, and {weekly} new functions pass review every week. Neither assumption is an observed work rate. Change either input to inspect the math.</p>
        <div className="ops-model-inputs"><label>Modeled C lanes<input type="number" min="1" max="20" value={lanes} onChange={event => setLanes(Math.max(1, Math.min(20, Number(event.target.value) || 1)))} /></label>
          <label>New verified functions / week<input type="number" min="1" max="500" value={weekly} onChange={event => setWeekly(Math.max(1, Math.min(500, Number(event.target.value) || 1)))} /></label></div>
        <div className="ops-model-result"><div><small>EQUAL POOL ALLOCATION</small><strong>{model.laneHourly == null ? 'Unknown' : money(model.laneHourly) + '/lane-hr'}</strong><span>Fixed calendar-hour pool ÷ {lanes} modeled lanes, all hours. This is not observed lane spend.</span></div>
          <div><small>TO WORKING CEILING</small><strong>{model.scenarioWeeks == null ? 'Unknown' : model.scenarioWeeks.toFixed(1) + ' weeks'}</strong><span>{model.remaining ?? '—'} remaining ÷ {weekly} newly credited per week. No evidence-backed ETA.</span></div></div>
        <p className="ops-rate-note">Required modeled pace: {(weekly / lanes).toFixed(2)} reviewed functions per lane-week if every modeled lane contributes equally.</p>
        <table className="ops-milestones"><thead><tr><th>Strict C milestone</th><th>More functions</th><th>Scenario weeks</th></tr></thead><tbody>{milestones.map(row => <tr key={row.share}><td>{Math.round(row.share * 100)}% · {row.target}/{model.ceiling}</td><td>+{row.additional}</td><td>{row.weeks.toFixed(1)}</td></tr>)}</tbody></table>
        <p className="ops-model-limit">At this assumed duration the fixed subscription commitment would be {money(model.scenarioCalendarCost)} if this month’s hourly rate held constant; this is not incremental C cost and excludes usage, taxes, and staffing. The working denominator can change with review.</p>
      </div></div>
    <div className="ops-next-evidence"><b>NEXT EVIDENCE GATES</b><span>Record verified strict C credit with timestamp and exact source head; measure C lane-hours and billable usage for actual spend; capture a same-run DOS/C frame for the playable demo. No defensible delivery date yet.</span></div>
  </section>;
}

function OpsSummary({ auto = true }) {
  const [snapshot, setSnapshot] = useState(null);
  const [history, setHistory] = useState(null);
  const [weekHistory, setWeekHistory] = useState(null);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  async function refresh() {
    setBusy(true); setError('');
    try {
      setSnapshot(await api('/ops-metrics/refresh', { method: 'POST' }));
      const [day, week] = await Promise.all([api('/ops-metrics/history'), api('/ops-metrics/history?hours=168')]);
      setHistory(day); setWeekHistory(week);
    }
    catch (exc) { setError(exc.message); }
    finally { setBusy(false); }
  }
  useEffect(() => {
    if (!auto) return undefined;
    let active = true;
    const load = () => Promise.all([api('/ops-metrics'), api('/ops-metrics/history'), api('/ops-metrics/history?hours=168')])
      .then(([data, timeline, week]) => { if (active) { setSnapshot(data); setHistory(timeline); setWeekHistory(week); setError(''); } })
      .catch(exc => { if (active) setError(exc.message); });
    load();
    const timer = setInterval(load, 60000);
    return () => { active = false; clearInterval(timer); };
  }, [auto]);
  const coderStates = snapshot?.staff?.coder || {};
  const coders = Object.values(coderStates).reduce((sum, count) => sum + count, 0);
  const progress = snapshot?.function_progress;
  const playable = snapshot?.goals?.find(goal => goal.goal_key === 'playable_c_demo');
  const quotas = snapshot?.quotas || {
    codex: snapshot?.quota?.provider === 'codex' ? snapshot.quota : null,
    claude: snapshot?.quota?.provider === 'claude' ? snapshot.quota : null,
  };
  const subscriptions = snapshot?.subscriptions || [];
  const opencodeUsage = snapshot?.opencode_usage;
  const hostHealth = snapshot?.host_health;
  const strictSeries = (history?.goal_observations || []).filter(row => row.goal_key === 'strict_fullc').map(row => ({ at: row.observed_at, value: Number(row.value) }));
  const identitySeries = (history?.goal_observations || []).filter(row => row.goal_key === 'verified_game_identities').map(row => ({ at: row.observed_at, value: Number(row.value) }));
  const coderSeries = (history?.coder_samples || []).map(row => ({ at: row.observed_at, value: row.working }));
  return <section className="dashboard-view ops-view" aria-label="Private operations dashboard">
    <div className="dashboard-tools"><span>Private sourced observations · scheduled sample every minute</span><button onClick={refresh} disabled={busy}>{busy ? 'Refreshing…' : 'Refresh data'}</button></div>
    {error && <div className="dashboard-error" role="alert">{error} <button onClick={refresh}>Try again</button></div>}
    <div className="ops-metrics"><div className="ops-heading"><strong>OPERATIONS METRICS</strong><small>{snapshot?.latest_sample_at ? 'Sample ' + new Date(snapshot.latest_sample_at).toLocaleTimeString() : 'Loading…'}</small></div>
    {snapshot && weekHistory && <RecreationLab snapshot={snapshot} history={weekHistory} />}
    {snapshot && weekHistory && <section className="ops-reference"><h2>Reviewed credit history and fixed-cost planning reference</h2><CoverageEconomics snapshot={snapshot} history={weekHistory} /></section>}
    {progress && <p className="ops-source-line">Published main {progress.commit_oid.slice(0, 8)} · checkpoint {new Date(progress.observed_at).toLocaleString()} · {progress.verified_game_identities} verified identities / {progress.ceiling} working ceiling; identity is separate from strict C credit.</p>}
    {history && <><div className="ops-section-head"><h2>Engineering flow · last 24 hours</h2><small>Recorded events and observations; no values are inferred between samples.</small></div>
      <div className="ops-charts">
        <LineTrend title="Reviewed strict C" points={strictSeries} color="#a9db7d" unit="functions" step note="Dated verified credit events; no credit from PR count or candidate rows." />
        <MergeTrend merges={history.pr_merges} start={history.window_start} end={history.generated_at} />
        <LineTrend title="Active coding agents" points={coderSeries} color="#83c6ed" unit="agents" note="Sampled process presence, not verified productive work; missing samples are gaps." />
        <LineTrend title="Verified game identities" points={identitySeries} color="#d6a6e8" unit="identities" step note="Dated verified identity events; distinct from strict C credit." />
      </div><p className="ops-dora">DORA deployment frequency, change failure rate, and recovery time: unavailable until deployment and incident records exist. PR merges above measure review throughput.</p></>}
    {snapshot && <div className="ops-grid">
      <div><b>STAFFING</b><strong>{coders} tracked coding agents</strong><small>{Object.entries(coderStates).map(([state, count]) => state + ' ' + count).join(' · ') || 'No coder sample'}</small></div>
      <div><b>PLAYABLE GAME GATE</b><strong>{playable?.status || 'Unverified'}</strong><small>{playable?.observed_at ? `Last manual checkpoint ${new Date(playable.observed_at).toLocaleString()} · ` : ''}A same-state DOS/C full frame is still required; no whole-game completion percentage is available</small></div>
      <QuotaCard provider="codex" quota={quotas.codex} />
      <QuotaCard provider="claude" quota={quotas.claude} />
      <div><b>FIXED SUBSCRIPTIONS</b><strong>{snapshot.monthly_fixed_usd == null ? 'Unavailable' : '$' + snapshot.monthly_fixed_usd.toFixed(2) + '/mo'}</strong><small>{subscriptions.map(item => item.provider + ' $' + item.monthly_usd.toFixed(2) + '/mo').join(' · ') || 'No owner cost record'} · owner reported</small></div>
      <div><b>OPENCODE MODEL COST</b><strong>{opencodeUsage ? '$' + opencodeUsage.month_estimated_usd.toFixed(4) + ' this month' : 'Unavailable'}</strong><small>{opencodeUsage ? `All recorded runs $${opencodeUsage.all_time_estimated_usd.toFixed(4)} · ${opencodeUsage.session_count} runs · observed ${new Date(opencodeUsage.observed_at).toLocaleString()} · ${opencodeUsage.scope}` : 'Local OpenCode cost source unavailable'} · no metered billing source</small></div>
      <div><b>HOST HEALTH</b><strong>{hostHealth ? `${hostHealth.stale ? 'STALE · ' : ''}/tmp ${Math.round(hostHealth.tmp_used_fraction * 100)}% · ${hostHealth.levels.tmp || 'unknown'}` : 'Unavailable'}</strong><small>{hostHealth ? `${hostHealth.tmp_free_gib.toFixed(2)} GiB free · inodes ${Math.round(hostHealth.tmp_inode_used_fraction * 100)}% · load ${hostHealth.load1.toFixed(1)}/${hostHealth.load5.toFixed(1)} on ${hostHealth.cpus} CPUs · CPU pressure ${hostHealth.cpu_psi_some_avg60.toFixed(1)}% · ${hostHealth.process_count ?? '—'} processes, ${hostHealth.zombie_count ?? '—'} zombies · observed ${Math.round(hostHealth.age_s / 60)} min ago` : 'Host watcher has no saved sample'}</small></div>
    </div>}</div>
  </section>;
}

function Fleet({ standalone = false }) {
  const [fleet, setFleet] = useState(null);
  const [error, setError] = useState('');
  const [selected, setSelected] = useState(null);
  const [requestedPane] = useState(() => new URLSearchParams(window.location.search).get('pane'));
  const [paneText, setPaneText] = useState(null);
  const [draft, setDraft] = useState('');
  const [sent, setSent] = useState('');
  const [sendError, setSendError] = useState('');
  const [sendBusy, setSendBusy] = useState(false);
  const sendingRef = useRef(false);
  const terminalRef = useRef(null);
  useEffect(() => { terminalRef.current?.scrollIntoView({ block: 'start' }); }, [selected]);

  async function load() {
    try {
      const data = await api('/fleet');
      if (data.state === 'unavailable') {
        setFleet(null); setSelected(null); setError(`Fleet unavailable: ${data.error || 'unknown reason'}`);
      } else {
        setFleet(data); setError('');
        if (requestedPane && data.rows?.some(row => row.pane_id === requestedPane)) {
          setSelected(current => current || requestedPane);
        }
      }
    } catch (e) { setFleet(null); setSelected(null); setError(`Fleet unavailable: ${e.message}`); }
  }
  useEffect(() => {
    load();
    const timer = setInterval(load, 10000);
    return () => clearInterval(timer);
  }, []);

  useEffect(() => {
    if (!selected) { setPaneText(null); return undefined; }
    let alive = true;
    async function loadText() {
      try {
        const data = await api(`/pane-text?pane=${encodeURIComponent(selected)}&lines=40`);
        if (alive) setPaneText(data);
      } catch (e) { if (alive) setPaneText({ pane: selected, text: '', reason: e.message }); }
    }
    loadText();
    const timer = setInterval(loadText, 3000);
    return () => { alive = false; clearInterval(timer); };
  }, [selected]);

  const projects = (fleet?.rows || []).reduce((groups, row) => { const project = row.project || 'Unassigned'; (groups[project] ||= []).push(row); return groups; }, {});
  const age = value => {
    if (value === null || value === undefined) return 'no activity recorded';
    if (value < 90) return `${value}s ago`;
    if (value < 5400) return `${Math.round(value / 60)}m ago`;
    return `${(value / 3600).toFixed(1)}h ago`;
  };

  async function send(event) {
    event.preventDefault();
    setSent(''); setSendError('');
    if (!draft.trim() || sendingRef.current) return;
    sendingRef.current = true; setSendBusy(true);
    try {
      const data = await api('/pane-send', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ pane: selected, text: draft }),
      });
      setDraft(current => current === draft ? '' : current);
      // The server performed the transport; it gets to describe it. Prefix the outcome so
      // 'stored' and 'unconfirmed' cannot be read as 'delivered'.
      setSent(sendOutcomeText(data));
    } catch (e) { setSendError(e.message); }
    finally { sendingRef.current = false; setSendBusy(false); }
  }

  return <DashShell standalone={standalone} view="fleet">
    <div className="dash-body fleet-body">
    <p className="dash-lead">Choose an agent to view or message.</p>
    {error && <p className="dash-error" role="alert">{error}</p>}
    {!fleet && !error && <p className="dash-empty">Loading agents…</p>}
    {fleet && !fleet.rows.length && <p className="dash-empty">No active agents right now.</p>}
    {Object.entries(projects).sort(([a], [b]) => a.localeCompare(b)).map(([project, rows]) =>
      <section key={project} className="fleet-project">
        <h2>{project} <span className="dash-muted">· {rows.length} lanes</span></h2>
        <div className="fleet-lanes">{rows.map(row => <button key={row.pane_id}
          aria-pressed={selected === row.pane_id} onClick={() => setSelected(row.pane_id)}>
          {laneDisplayName(row)} <small>{row.agent} · {age(row.last_activity_s)}</small>
          {(row.pending_state === 'unknown' || row.pending == null) && <small>Pending: UNKNOWN</small>}
        </button>)}</div>
      </section>)}
    <section className="dash-terminal" aria-label="Unrouted agent processes">
      <h2>Agent processes without a desk route</h2>
      {fleet?.unrouted?.state !== 'ok'
        ? <p className="dash-muted">{fleet?.unrouted?.reason || 'Process inventory unavailable'}</p>
        : <>
          <p className="dash-muted">{fleet.unrouted.note}</p>
          {!fleet.unrouted.rows.length && <p>No unrouted agent processes detected.</p>}
          {fleet.unrouted.rows.map(row => <p key={row.pid}>
            <strong>{row.project}</strong> · {row.agent} · PID {row.pid}
            <br />{row.reason} · Session write: {age(row.last_session_write_s)}
            {row.session_age_reason ? ' · ' + row.session_age_reason : ''}
          </p>)}
        </>}
    </section>
    {selected && <section ref={terminalRef} className="dash-terminal" data-testid="pane-text">
      <h2>{laneDisplayName((fleet?.rows || []).find(row => row.pane_id === selected))} · Terminal</h2>
      {(() => {
        const lane = (fleet?.rows || []).find(r => r.pane_id === selected);
        if (!lane) return null;
        return <p className="dash-muted" role="status" data-testid="pending-status">
          {lane.pending_state === 'unknown' || lane.pending == null
            ? 'Pending: UNKNOWN — message receipt counts could not be read.'
            : `${lane.pending} message(s) awaiting receipt.`}
        </p>;
      })()}
      {(() => {
        const lane = (fleet?.rows || []).find(r => r.pane_id === selected);
        if (!lane || lane.send !== 'guarded' || lane.can_type) return null;
        return <p className="dash-error" role="status">
          Messaging is unavailable for this agent. Ask the desk to restore it.
        </p>;
      })()}
      <form onSubmit={send}>
        <label htmlFor="lane-draft">Message this agent</label>
        <textarea id="lane-draft" rows={3} value={draft}
          onChange={event => setDraft(event.target.value)}
          placeholder="Write a message" />
        {(() => {
          const lane = (fleet?.rows || []).find(r => r.pane_id === selected);
          const needsToken = lane && lane.send !== 'owner_push';
          const blocked = needsToken && !lane.can_type;
          return <button type="submit" disabled={sendBusy || !draft.trim() || blocked}
            title={blocked
              ? 'Messaging is unavailable for this agent'
              : 'Send message'}>{sendBusy ? 'Sending…' : 'Send'}</button>;
        })()}
        {sent && <p className="dash-muted" role="status">{sent}</p>}
        {sendError && <p className="dash-error">{sendError}</p>}
      </form>
      <p className="dash-muted">Current screen only. Full-screen app history is not captured.</p>
      {paneText && paneText.reason && <p className="dash-error">{paneText.reason}</p>}
      {paneText && !paneText.reason && <pre className="dash-terminal-text" tabIndex={0} aria-label="Current terminal screen">{paneText.text}</pre>}
      {!paneText && <p className="dash-empty">Loading terminal…</p>}
    </section>}
    </div>
  </DashShell>;
}

function Budget({ data }) {
  const percent = value => value == null ? 'unknown' : `${value.toFixed(1)}%`;
  const time = value => value ? new Date(value).toLocaleString() : 'unknown';
  const age = provider => {
    if (!provider.measured_at) return 'reading age unknown';
    const seconds = Math.max(0, provider.age_s || 0, Math.floor((Date.now() - new Date(provider.measured_at).getTime()) / 1000));
    const elapsed = seconds < 60 ? seconds : Math.floor(seconds / 60);
    return (provider.state === 'error' && !provider.last_good ? 'checked ' : 'read ') + elapsed + (seconds < 60 ? ' sec ago' : ' min ago');
  };
  return <section className="ops-budget" data-ops-section="budget">
    <h2>Provider budgets</h2>
    {data.error && <p className="dash-error">Budget source unavailable: {data.error}</p>}
    {data.subscription_error && <p>{data.subscription_error}</p>}
    {data.providers.map(provider => <div className="budget-provider" key={provider.provider}>
      <div className="budget-provider-head"><strong>{provider.provider}</strong>
        <span>{provider.subscription ? `$${provider.subscription.monthly_usd.toFixed(2)}/month subscription` : 'Subscription cost unknown'}</span>
      </div>
      <small title={`${provider.source} · generated ${data.generated_at}`}>
        {provider.state === 'not_read_yet' ? 'not read yet' : provider.state} · {age(provider)} · measured {time(provider.measured_at)}
        {provider.reason ? ` · ${provider.reason}` : ''}
        {provider.failed_at ? ' · failed ' + time(provider.failed_at) : ''}
      </small>
      {provider.subscription && <small>Subscription source: {provider.subscription.source} · observed {time(provider.subscription.observed_at)}</small>}
      {provider.meaning === 'local_session_estimate'
        ? <p>Local session estimate: {provider.local_estimate?.session_cost_usd == null ? 'unknown' : `$${provider.local_estimate.session_cost_usd.toFixed(4)}`} · {provider.local_estimate?.last_context_tokens?.toLocaleString() ?? 'unknown'} context tokens · {provider.local_estimate?.session_messages_with_usage?.toLocaleString() ?? 'unknown'} messages. Not plan quota or a bill.</p>
        : provider.windows.length
          ? <ul>{provider.windows.map((window, i) => <li key={i}>
              <b>{provider.last_good ? 'Last reading · ' : ''}{window.name}</b>: <strong>{percent(window.percent_remaining)} left</strong> · {percent(window.percent_used)} used
              · resets {time(window.resets_at)}{window.state !== 'ok' ? ` · ${window.state}` : ''}
            </li>)}</ul>
          : <p>No window data — remaining allowance unknown.</p>}
      {provider.extra_usage && <p>Extra usage: {provider.extra_usage.used_credits ?? 'unknown'} / {provider.extra_usage.monthly_limit ?? 'unknown'} credits used
        · utilization {percent(provider.extra_usage.utilization)}
        · spend_limit_reached: {String(provider.extra_usage.spend_limit_reached ?? 'unknown')}</p>}
    </div>)}
    <p className="dash-muted">{data.dollars_reason}</p>
  </section>;
}

function NeedsOwner({ rows, asks, onAnswer, onNotNow, busy, revision }) {
  // DESK_DESIGN 4.1 (t997): the questions the owner owes an answer to, at the top of the
  // desk. Each row shows the LANE'S QUESTION VERBATIM (the board carries it, the desk does
  // not paraphrase), its age and what an answer unblocks. Collapses to one line when there
  // is nothing waiting, because an empty panel that still takes a screen is a lie about
  // how much there is to do.
  const [reply, setReply] = useState({});
  const [questions, setQuestions] = useState({});
  const rowIds = rows.map(row => row.id).join(',');
  useEffect(() => {
    let active = true;
    Promise.all(rows.map(async row => {
      try {
        const q = await api('/ops-board/question', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ task_id: row.id }) });
        return [row.id, q];
      } catch (error) { return [row.id, { error: error.message }]; }
    })).then(entries => { if (active) setQuestions(Object.fromEntries(entries)); });
    return () => { active = false; };
  }, [rowIds, revision]);
  const age = seconds => seconds == null ? 'unknown' : seconds < 3600
    ? `${Math.floor(seconds / 60)}m` : `${Math.floor(seconds / 3600)}h`;
  if (!rows.length) return <p className="ops-owner-empty" data-ops-section="needs-owner">Nothing needs your decision.</p>;
  return <section className="ops-owner" data-ops-section="needs-owner" aria-label="Decisions you owe">
    <h2>Decisions you owe <span className="ops-owner-count">{rows.length}</span></h2>
    <ul>
      {rows.map(row => <li key={row.id}>
        <div className="ops-owner-line">
          <code>{row.id}</code>
          <span className="ops-owner-question">{questions[row.id]?.question || row.needs_owner_question}</span>
          <span className="ops-owner-age" title={`waiting ${age(row.age_s)}`}>{age(row.age_s)}</span>
        </div>
        {row.blocks?.length > 0 && <p className="ops-owner-blocks">
          unblocks {row.blocks.map(id => <code key={id}>{id}</code>)}
        </p>}
        <div className="ops-owner-reply">
          {questions[row.id]?.error && <span role="alert">{questions[row.id].error}</span>}
          {questions[row.id] && !questions[row.id].error && (questions[row.id].options?.length > 0
            ? questions[row.id].options.map(option => <button key={option.number} disabled={busy}
                onClick={() => onAnswer(row.id, { version: questions[row.id].version, option_number: option.number })}>{option.number}: {option.label}</button>)
            : <><input aria-label={`Answer ${row.id}`} placeholder="Answer the lane…"
                 value={reply[row.id] || ''} onChange={event => setReply(state => ({ ...state, [row.id]: event.target.value }))} />
              <button disabled={busy || !(reply[row.id] || '').trim()} onClick={() => onAnswer(row.id, { version: questions[row.id].version, reply: reply[row.id] })}>Answer</button></>)}
          <button className="ops-owner-notnow" disabled={busy} onClick={() => onNotNow(row.id)}>Not now</button>
        </div>
      </li>)}
    </ul>
    {asks?.length > 0 && <div className="ops-owner-asks">
      <h3>What I asked for <span className="dash-meta">last 24h</span></h3>
      <ul>{asks.map(ask => <li key={ask.id}>
        <code>{ask.id}</code> <span className={`ops-chip ops-chip-${ask.status}`}>{ask.status}</span>
        <span className="ops-owner-question">{ask.title}</span>
        <span className="ops-owner-age">{age(ask.age_s)}</span>
      </li>)}</ul>
    </div>}
  </section>;
}

function OpsBoard({ standalone = false }) {
  const poolrad = usePoolradProgress();
  const [board, setBoard] = useState(null);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const [acting, setActing] = useState(false);
  async function answer(taskId, decision) {
    setActing(true);
    try { await api('/ops-board/answer', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ task_id: taskId, ...decision }) }); await load(true); }
    catch (e) { setError(e.message); } finally { setActing(false); }
  }
  async function notNow(taskId) {
    setActing(true);
    try { await api('/ops-board/not-now', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ task_id: taskId }) }); await load(true); }
    catch (e) { setError(e.message); } finally { setActing(false); }
  }
  async function load(force = false) {
    setBusy(true);
    try {
      setBoard(await api(force ? '/ops-board/refresh' : '/ops-board', force ? { method: 'POST' } : {}));
      setError('');
    } catch (e) { setError(e.message); } finally { setBusy(false); }
  }
  useEffect(() => { load(); const timer = setInterval(load, 15000); return () => clearInterval(timer); }, []);
  const taskRank = row => row.status === 'blocked' ? 0 : row.assignees.includes('owner') ? 1 : row.status === 'in_progress' ? 2 : 3;
  const taskRows = [...(board?.reference.tasks.items || [])].sort((a, b) => taskRank(a) - taskRank(b));
  const taskAge = seconds => seconds == null ? 'unknown' : seconds < 3600 ? Math.floor(seconds / 60) + 'm' : Math.floor(seconds / 3600) + 'h';
  const tasks = board?.brief.tasks;
  const lanes = board?.brief.lanes;
  const alarms = board?.attention.alarms || [];
  const owner = board?.attention.owner;
  const evidence = source => source ? `${source.source}: ${source.state} · generated ${source.generated_at}` : '';
  const number = (source, key) => source?.state === 'ok' ? source[key] ?? 'unknown' : 'unavailable';
  return <DashShell standalone={standalone} view="operations">
    <DashHeader title="Operations" view="operations" standalone={standalone} side={<>
      <span className="dash-meta">{board ? `generated ${new Date(board.generated_at).toLocaleString()}` : 'Loading…'}</span>
      <button onClick={() => { load(true); poolrad.load(); }} disabled={busy}>Refresh</button>
    </>} />
    <div className="dash-body ops-body">
      {error && <p className="dash-error" role="alert">{error}</p>}
      {board?.refresh_error && <p className="dash-error" role="alert">Refresh failed; showing the last snapshot.</p>}
      {board && <>
        <NeedsOwner rows={(board.reference?.tasks?.items || []).filter(row => row.needs_owner_question)}
                    asks={board.reference?.asks?.asks || []}
                    onAnswer={answer} onNotNow={notNow} busy={acting} revision={board.generated_at} />
        <section className="ops-brief" data-ops-section="brief">
          <strong>{alarms.length ? `${alarms.length} alarms` : 'Alarms: none'}</strong>
          <p title={evidence(tasks)}>{number(tasks, 'open')} open tasks · {number(tasks, 'owner')} assigned to you</p>
          <p title={evidence(tasks)}>{leaderLine(tasks, taskAge)} · the leader's carried global work (unowned or assigned to the leader)</p>
          <p title={evidence(lanes)}>{number(lanes, 'working')} lanes working · {number(lanes, 'idle')} idle{lanes?.unknown ? ` · ${lanes.unknown} not reporting` : ''}</p>
        </section>
        <p id="poolrad" className="ops-poolrad">
          <b>Poolrad:</b> {poolrad.data
            ? <>{poolrad.data.strict_full_c ?? 'unknown'}/{poolrad.data.ceiling ?? 'unknown'} credited strict C ({poolrad.data.strict_percent ?? 'unknown'}%) · {poolrad.data.identities ?? 'unknown'}/{poolrad.data.ceiling ?? 'unknown'} identities ({poolrad.data.identity_percent ?? 'unknown'}%) · CEILING {poolrad.data.ceiling ?? 'unknown'} ({String(poolrad.data.bound_kind || 'unknown').replaceAll('_', ' ')}) · {poolrad.data.replacements?.count ?? 'unknown'} recorded replacements · not whole-game completion. </>
            : poolrad.error ? 'Source unavailable. ' : 'Loading checkpoint… '}
          {poolrad.error && poolrad.data && <span>Refresh failed; showing previous checkpoint. </span>}
          <a href="#poolrad-details">Evidence and timestamps</a>
        </p>
        <section data-ops-section="attention" className="ops-attention">
          {alarms.slice(0, 3).map((alarm, i) => <p key={i} title={evidence(alarm)}>{alarm.source}: {alarm.text}</p>)}
          {alarms.length > 3 && <p>More alarms in the reference below.</p>}
          {owner?.items.slice(0, 3).map(task => <p key={task.id} title={evidence(owner)}><b>{task.id}</b> {task.title}</p>)}
          {owner?.state !== 'ok' && <p>Owner task assignments unavailable.</p>}
          {owner?.state === 'ok' && !owner.items.length && <p className="dash-muted">No tasks explicitly assigned to you.</p>}
        </section>
        {board.poolrad_progress && <section className="ops-poolrad-tile" data-ops-section="poolrad" title={evidence(board.poolrad_progress)}>
          <h2>Poolrad progress</h2>
          {board.poolrad_progress.state === 'ok'
            ? <p>{board.poolrad_progress.credited ?? 'unknown'} credited / {board.poolrad_progress.identities ?? 'unknown'} identities · CEILING {board.poolrad_progress.ceiling ?? 'unknown'} <small>{board.poolrad_progress.ceiling_note}</small></p>
            : <p>{board.poolrad_progress.error || 'Unavailable'}</p>}
        </section>}
        {board.budget && <Budget data={board.budget} />}
        {board.reports && <section className="ops-reports" data-ops-section="reports" title={evidence(board.reports)}>
          <h2>Reports</h2>
          {board.reports.state !== 'ok' ? <p>{board.reports.error || 'Unavailable'}</p>
            : board.reports.items.map(r => <details key={r.name} open={r.open}>
              <summary>{r.name} <small>{new Date(r.at * 1000).toLocaleString()}</small></summary>
              <p className="dash-muted">{r.size} bytes · body is not served over the daemon socket yet</p>
            </details>)}
        </section>}
        <section className="ops-reference" data-ops-section="reference">
          <h2>Reference</h2>
          <PoolradReference data={poolrad.data} error={poolrad.error} />
          {alarms.length > 3 && <ul>{alarms.map((alarm, i) => <li key={i} title={evidence(alarm)}>{alarm.source}: {alarm.text}</li>)}</ul>}
          {[
            ['Tasks', {...board.reference.tasks, items: taskRows}, ['ID', 'Task / status', 'Age', 'Assignee'],
              row => [row.id, <>{row.title}<small>{row.status}</small></>, taskAge(row.age_s), row.assignees.join(', ') || '—']],
            ['Lanes', board.reference.lanes, ['Pane', 'Lane', 'Status'],
              row => [row.pane_id, row.tab_label, <LaneStatus row={row} />]],
            ['Schedules', board.reference.schedules, ['ID', 'Schedule', 'Target', 'Status'],
              row => [row.id, row.name, row.target_label || row.target, row.running ? 'running' : 'stopped']],
          ].map(([name, source, columns, cells]) => <div key={name} className="ops-table">
            <h2>{name}</h2><small>{evidence(source)}</small>
            {source.state !== 'ok' ? <p>{source.error || 'Unavailable'}</p> :
              <div className="dash-table-wrap"><table><thead><tr>{columns.map(c => <th key={c}>{c}</th>)}</tr></thead>
                <tbody>{source.items.map((row, i) => <tr key={row.id || row.pane_id || i}>{cells(row).map((v, j) => <td key={j}>{v}</td>)}</tr>)}</tbody>
              </table></div>}
          </div>)}
        </section>
      </>}
    </div>
  </DashShell>;
}
export default OpsBoard;
