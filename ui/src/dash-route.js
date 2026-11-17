// The dashboards the desk serves and which one a path opens (t4642). Projects is the landing
// view: '/', '/dash' and the DS brand link open it. Inbox stays at /dash/cos. The order here
// is the order of the navigation links.
export const DASH_ROUTES = {
  projects: { eyebrow: 'PROJECTS', title: 'Projects', subtitle: 'Live lanes by explicit project membership' },
  cos: { eyebrow: 'INBOX', title: 'Inbox', subtitle: 'Browse Your inbox and VP history' },
  play: { eyebrow: 'LIVE GAME', title: 'Play Pool of Radiance', subtitle: 'Private DOS run · save states' },
};

export const DEFAULT_DASH_VIEW = 'projects';
export const DASH_HOME = `/dash/${DEFAULT_DASH_VIEW}`;

// Retired dashboards and where their bookmarks land (t4646). The separate Terminals view is
// gone: lane terminals and messages live in Projects. The query string is kept, so
// /dash/fleet?pane=%7 opens that lane's terminal in Projects.
export const DASH_REDIRECTS = { fleet: 'projects', operations: 'projects', databases: 'projects', knowledge: 'projects', tasks: 'projects' };

// `standalone` is false only outside /dash (the bare '/' entry), where the default view is shown.
export function resolveDashRoute(path) {
  const match = /^\/dash\/([a-z]+)\/?$/.exec(path);
  const standalone = path === '/dash' || path.startsWith('/dash/');
  const named = match ? match[1] : DEFAULT_DASH_VIEW;
  const view = Object.hasOwn(DASH_REDIRECTS, named) ? DASH_REDIRECTS[named] : named;
  return { standalone, view, known: Object.hasOwn(DASH_ROUTES, view) };
}

// The address a retired dashboard path should be replaced with, query string kept, or null when
// the path needs no change.
export function canonicalDashLocation(path, search = '', hash = '') {
  const params = new URLSearchParams(search);
  if (/^\/dash\/cos\/?$/.test(path) && params.get('panel') === 'webmail') {
    params.delete('panel');
    return `/dash/cos${params.size ? `?${params}` : ''}${hash}`;
  }
  if (/^\/dash\/(knowledge|databases|tasks)\/?$/.test(path)) {
    if (path.includes('tasks')) params.set('panel', 'vp-tasks');
    else if (params.get('project') && params.get('project') !== 'global') params.set('panel', 'knowledge');
    else { params.delete('project'); params.set('panel', 'vp-knowledge'); params.set('store', 'global'); }
    return `/dash/projects?${params}${hash}`;
  }
  const match = /^\/dash\/([a-z]+)\/?$/.exec(path);
  if (!match || !Object.hasOwn(DASH_REDIRECTS, match[1])) return null;
  return `/dash/${DASH_REDIRECTS[match[1]]}${typeof search === 'string' ? search : ''}${typeof hash === 'string' ? hash : ''}`;
}

// The lane a deep link asks for: ?pane=%<digits>, nothing else.
export function requestedPane(search) {
  const pane = new URLSearchParams(typeof search === 'string' ? search : '').get('pane');
  return typeof pane === 'string' && /^%\d+$/.test(pane) ? pane : null;
}

// What the app renders for a path: the default view anywhere outside /dash.
export function viewForPath(path) {
  const route = resolveDashRoute(path);
  return route.standalone ? { view: route.view, known: route.known } : { view: DEFAULT_DASH_VIEW, known: true };
}
