// Pane identity comes from the authenticated, server-filtered terminal list.
export function terminalTargetForPane(targets, pane) {
  if (typeof pane !== 'string' || !/^%\d+$/.test(pane) || !Array.isArray(targets)) return null;
  const matches = targets.filter(target => target?.pane_id === pane && typeof target.name === 'string' && target.name.length > 0);
  return matches.length === 1 ? matches[0] : null;
}
