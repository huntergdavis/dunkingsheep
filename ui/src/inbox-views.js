// Display names are separate from recorded, daemon-validated routing/lineage facts.
export const INBOX_VIEWS = {
  human_threads: { label: 'Your inbox', source: 'human', description: 'Conversations with you, including replies.' },
  vp_threads: { label: 'VP', source: 'vp', description: 'Agent coordination with VP of Agentry.' },
  historical: { label: 'Historical / prior versions', source: 'historical', description: 'Messages from prior versions. Original content is preserved; no thread links are inferred.' },
};
const lineageFields = ['thread_version', 'parent_id', 'root_id', 'conversation_kind', 'sender_kind'];
const messageId = value => typeof value === 'string' && /^i[1-9][0-9]*$/.test(value);
export function validateInboxPage(data, viewName) {
  const view = INBOX_VIEWS[viewName];
  if (!view || !data || !Object.hasOwn(data, 'next_before') || data.source_filter !== viewName || data.include_internal !== false || !Array.isArray(data.items)) {
    throw new Error('This inbox view is unavailable. Try refreshing after the service is updated.');
  }
  for (const item of data.items) {
    const scoped = item.conversation_source === view.source && Object.hasOwn(item, 'audience') && (item.audience === null || item.audience === 'agent');
    const versioned = item.thread_version === 1 && item.conversation_kind === view.source && messageId(item.root_id) && (item.parent_id === null || messageId(item.parent_id)) && ['human', 'agent', 'system'].includes(item.sender_kind);
    const historical = lineageFields.every(field => Object.hasOwn(item, field) && item[field] === null);
    if (!scoped || !(viewName === 'historical' ? historical : versioned)) {
      throw new Error('Messages for this inbox could not be verified. Try refreshing.');
    }
  }
  return { ...data, pagingAvailable: true };
}
