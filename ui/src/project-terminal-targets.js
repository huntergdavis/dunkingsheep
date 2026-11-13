// Pane identity comes from the authenticated, server-filtered terminal list.
export function terminalTargetForPane(targets, pane) {
  if (typeof pane !== 'string' || !/^%\d+$/.test(pane) || !Array.isArray(targets)) return null;
  const matches = targets.filter(target => target?.pane_id === pane && typeof target.name === 'string' && target.name.length > 0);
  return matches.length === 1 ? matches[0] : null;
}

export const OWNER_TERMINAL_NAME = '__owner_terminal__';

// Why the whole terminal list is missing, in the owner's words (t4643).
export function terminalListReason(status) {
  if (status === 404) return 'Terminals are switched off on this desk';
  if (status === 401 || status === 403) return 'Sign in again to open terminals';
  if (status === 503) return 'Terminal list unavailable: the desk could not read its lane records';
  return 'Terminal list unavailable';
}

// What a lane row offers: a terminal the server authorized for exactly that pane, or the reason
// there is none. `list` is { state: 'loading' | 'ready' | 'error', targets, status }. The server
// decides eligibility; this never invents a target.
export function terminalActionForLane(list, pane) {
  if (!list || list.state === 'loading') return { target: null, reason: 'Checking terminal access…' };
  if (list.state === 'error') return { target: null, reason: terminalListReason(list.status) };
  const target = terminalTargetForPane(list.targets, pane);
  if (!target) {
    return { target: null, reason: 'No terminal: not a named, registered Dunking Sheep lane, or relaunched since it registered' };
  }
  return target.name === OWNER_TERMINAL_NAME
    ? { target: { ...target, label: 'Owner' }, label: 'Open owner terminal', reason: null }
    : { target, label: 'Open terminal', reason: null };
}
