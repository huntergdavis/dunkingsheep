export function buildThemeRegistry(manifests, styles) {
  const seen = new Set();
  const themes = Object.entries(manifests).map(([file, theme]) => {
    const id = file.split('/').pop().replace(/\.json$/, '');
    if (!theme || theme.id !== id || !/^[a-z0-9][a-z0-9-]*$/.test(id) ||
        typeof theme.name !== 'string' || !theme.name.trim() ||
        !Number.isFinite(theme.order) || seen.has(id) ||
        (theme.group !== undefined && (typeof theme.group !== 'string' || !theme.group.trim()))) {
      throw new Error('Invalid theme manifest: ' + file);
    }
    if (!(file.replace(/\.json$/, '.css') in styles)) {
      throw new Error('Theme stylesheet missing: ' + id);
    }
    seen.add(id);
    const row = { id, name: theme.name, order: theme.order };
    if (theme.group !== undefined) row.group = theme.group.trim();
    return row;
  });
  for (const file of Object.keys(styles)) {
    if (!(file.replace(/\.css$/, '.json') in manifests)) {
      throw new Error('Theme manifest missing: ' + file);
    }
  }
  if (!seen.has('default')) throw new Error('Default theme is required');
  const compare = (a, b) => a < b ? -1 : a > b ? 1 : 0;
  return themes.sort((a, b) => Number(b.id === 'default') - Number(a.id === 'default') ||
    a.order - b.order || compare(a.name, b.name) || compare(a.id, b.id));
}

// Group labels in their declared order; labels not pinned here sort alphabetically after.
export const THEME_GROUP_ORDER = ['Standard', 'Classic sci-fi', 'Other'];

export function groupThemes(themes, pinnedOrder = THEME_GROUP_ORDER) {
  const buckets = new Map();
  for (const theme of themes) {
    const label = theme.group || 'Other';
    if (!buckets.has(label)) buckets.set(label, []);
    buckets.get(label).push(theme);
  }
  const rank = label => {
    const at = pinnedOrder.indexOf(label);
    return at === -1 ? pinnedOrder.length : at;
  };
  const compare = (a, b) => a < b ? -1 : a > b ? 1 : 0;
  return [...buckets.keys()].sort((a, b) => rank(a) - rank(b) || compare(a, b))
    .map(label => ({ label, options: buckets.get(label) }));
}
