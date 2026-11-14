// Match ds_core::inbox::normalise_mailbox: only a trailing pane ID is an alias.
// Keep the persisted key on each message for exact body lookups.
export function normaliseMailbox(name) {
  const text = typeof name === 'string' ? name.trim() : '';
  const plain = text.replace(/\s*\(\s*(?:[A-Za-z0-9_.-]+:p[A-Za-z0-9_.-]+|%[0-9]+)\s*\)$/, '').trimEnd();
  return plain || text;
}
