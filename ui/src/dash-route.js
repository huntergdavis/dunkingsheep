// The dashboards the desk serves and which one a path opens (t4642). Projects is the landing
// view: '/', '/dash' and the DS brand link open it. Front Desk stays at /dash/cos. The order here
// is the order of the navigation links.
export const DASH_ROUTES = {
  projects: { eyebrow: 'PROJECTS', title: 'Projects', subtitle: 'Live lanes by explicit project membership' },
  cos: { eyebrow: 'FRONT DESK', title: 'Front Desk', subtitle: 'Ask; decisions you owe are at the top' },
  fleet: { eyebrow: 'TERMINALS', title: 'Terminals', subtitle: 'See active agents and send a message' },
  operations: { eyebrow: 'OPERATIONS', title: 'Operations board', subtitle: 'Tasks, progress, and service health' },
  play: { eyebrow: 'LIVE GAME', title: 'Play Pool of Radiance', subtitle: 'Private DOS run · save states' },
};

export const DEFAULT_DASH_VIEW = 'projects';
export const DASH_HOME = `/dash/${DEFAULT_DASH_VIEW}`;

// `standalone` is false only outside /dash (the bare '/' entry), where the default view is shown.
export function resolveDashRoute(path) {
  const match = /^\/dash\/([a-z]+)\/?$/.exec(path);
  const standalone = path === '/dash' || path.startsWith('/dash/');
  const view = match ? match[1] : DEFAULT_DASH_VIEW;
  return { standalone, view, known: Object.hasOwn(DASH_ROUTES, view) };
}

// What the app renders for a path: the default view anywhere outside /dash.
export function viewForPath(path) {
  const route = resolveDashRoute(path);
  return route.standalone ? { view: route.view, known: route.known } : { view: DEFAULT_DASH_VIEW, known: true };
}
