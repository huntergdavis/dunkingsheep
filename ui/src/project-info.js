import { formatTimestamp } from './timestamps.js';
// The project information browser (t3250): pure helpers for the desk panel.

export const CLASS_LABELS = {
  'ground-truth': 'Ground truth',
  fact: 'Fact',
  provisional: 'Provisional',
  history: 'History',
};

// A short user-facing sentence for a store view or list state that has no entries to show.
export function viewStateText(view) {
  if (!view) return 'Loading…';
  switch (view.state) {
    case 'ok': return '';
    case 'empty': return 'This store has no entries yet.';
    case 'no_match': return 'No entry matches the search.';
    case 'not_mapped': return 'No information store is mapped for this project.';
    case 'mapping_invalid': return `The mapping for this project is invalid: ${view.reason || 'no reason given'}.`;
    case 'map_invalid': return `The project map cannot be used: ${view.reason || 'no reason given'}.`;
    case 'no_store': return 'There is no global information store.';
    case 'unavailable': return `Information is unavailable: ${view.reason || 'the desk could not read it'}.`;
    default: return 'Unknown state.';
  }
}

// The provenance line of an entry: type, authority, source, and its standing.
export function provenance(entry, timeZone) {
  const parts = [];
  if (entry.type) parts.push(entry.type);
  if (entry.authority) parts.push(`authority: ${entry.authority}`);
  if (entry.source) parts.push(`source: ${entry.source}`);
  if (entry.stated) parts.push(`stated ${formatTimestamp(entry.stated, timeZone)}`);
  return parts.join(' · ');
}

// The warnings an entry carries, most important first.
export function standing(entry) {
  const marks = [];
  if (entry.error) marks.push(`unreadable: ${entry.error}`);
  if (entry.retired) marks.push('retired');
  if (entry.superseded_by) marks.push(`superseded by ${entry.superseded_by}`);
  if (entry.stale) marks.push('stale: re-check before trusting');
  return marks;
}

// Entries split for display: ground truths apart from everything else, order kept.
export function splitEntries(entries) {
  const list = Array.isArray(entries) ? entries : [];
  return {
    groundTruths: list.filter(entry => entry.ground_truth === true),
    others: list.filter(entry => entry.ground_truth !== true),
  };
}

// t5241: the store a Databases deep link asks for: ?project=global, or ?project=<name>.
// Anything else (missing, empty, untrimmed) selects nothing.
export function requestedStore(search) {
  const project = new URLSearchParams(typeof search === 'string' ? search : '').get('project');
  if (typeof project !== 'string' || !project || project.trim() !== project) return '';
  return project === 'global' ? 'global' : `project:${project}`;
}

// The query string that reopens `selected` ('' for none), for the address bar.
export function storeSearch(selected) {
  if (!selected) return '';
  const project = selected === 'global' ? 'global' : selected.slice('project:'.length);
  return `?project=${encodeURIComponent(project)}`;
}

// The Databases address for a project's information (the link in Projects).
export function informationHref(project) {
  return `/dash/knowledge${storeSearch(`project:${project}`)}`;
}
