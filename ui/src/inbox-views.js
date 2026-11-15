// Display names are separate from recorded, daemon-validated routing/lineage facts.
export const INBOX_VIEWS = {
  human_threads: { label: 'Your inbox', source: 'human', description: 'Conversations with you, including replies.' },
  vp_threads: { label: 'VP', source: 'vp', description: 'Agent coordination with VP of Agentry.' },
  historical: { label: 'Historical / prior versions', source: 'historical', description: 'Prior-version and unclassified messages. Original content is preserved; no thread links are inferred.' },
};
export const HISTORICAL_REASONS = { prior_version: 'Prior version', other: 'Other conversation', unknown: 'Unrecognised conversation metadata' };
const historicalLabels = new Set(['Agent-only', 'Routine agent mail', 'Scheduled check-in']);
const kindLabels = new Set(['System', 'Alert', 'Agent-only']);
export function messageKindLabels(item) {
  return [...new Set([...(item.sender_kind === 'system' ? ['System'] : []), ...(item.audience === 'agent' ? ['Agent-only'] : []), ...(item.kind_labels || [])])];
}
const lineageFields = ['thread_version', 'parent_id', 'root_id', 'conversation_kind', 'sender_kind'];
const messageId = value => typeof value === 'string' && /^i[1-9][0-9]*$/.test(value);
export function validateInboxPage(data, viewName) {
  const view = INBOX_VIEWS[viewName];
  if (!view || !data || !Object.hasOwn(data, 'next_before') || data.source_filter !== viewName || data.include_internal !== false || !Array.isArray(data.items)) {
    throw new Error('This inbox view is unavailable. Try refreshing after the service is updated.');
  }
  if (viewName !== 'historical' && (!Number.isSafeInteger(data.view_revision) || data.view_revision < 0)) throw new Error('This inbox revision could not be verified. Try refreshing.');
  for (const item of data.items) {
    const scoped = item.conversation_source === view.source && Object.hasOwn(item, 'audience') && (item.audience === null || item.audience === 'agent');
    const kindMatches = item.conversation_kind === view.source || viewName === 'human_threads' && item.conversation_kind === 'vp' && item.sender_kind === 'agent' && item.recipient_route_kind === 'human';
    const validLabels = item.kind_labels === undefined || Array.isArray(item.kind_labels) && item.kind_labels.every(label => kindLabels.has(label));
    const versioned = validLabels && item.thread_version === 1 && kindMatches && messageId(item.root_id) && (item.parent_id === null || messageId(item.parent_id)) && ['human', 'agent', 'system'].includes(item.sender_kind);
    const historical = lineageFields.every(field => Object.hasOwn(item, field) && item[field] === null) && Object.hasOwn(HISTORICAL_REASONS, item.historical_reason) && Array.isArray(item.historical_labels) && item.historical_labels.every(label => historicalLabels.has(label));
    if (!scoped || !(viewName === 'historical' ? historical : versioned)) {
      throw new Error('Messages for this inbox could not be verified. Try refreshing.');
    }
  }
  return { ...data, pagingAvailable: true };
}
