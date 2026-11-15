import './viewportReport';
import React, { useEffect, useLayoutEffect, useState, lazy, Suspense } from 'react';
import { createRoot } from 'react-dom/client';
import '@chatscope/chat-ui-kit-styles/dist/default/styles.min.css';
import './style.css';
import './dash.css';
import './scrollbars.css';
import { canonicalDashLocation, DASH_ROUTES, resolveDashRoute, viewForPath } from './dash-route.js';
import { ThemeContext, validTheme, storedTheme, api, DashShell, Login } from './DeskShared';
const OwnerInbox = lazy(() => import('./OwnerInbox'));
const MailboxBrowser = lazy(() => import('./MailboxBrowser'));
const TasksView = lazy(() => import('./TasksView'));
const ProjectInformation = lazy(() => import('./ProjectInformation'));
const Projects = lazy(() => import('./ProjectsView'));
const OpsBoard = lazy(() => import('./OperationsView'));
const PlayWorkspace = lazy(() => import('./PlayView'));
const ChannelStandalone = lazy(() => import('./ConversationView'));

function useDashRoute() {
  const [location, setLocation] = useState(() => ({ path: window.location.pathname, search: window.location.search }));
  useEffect(() => {
    const update = () => {
      const moved = canonicalDashLocation(window.location.pathname, window.location.search, window.location.hash);
      if (moved) window.history.replaceState(null, '', moved);
      setLocation({ path: window.location.pathname, search: window.location.search });
    };
    update();
    window.addEventListener('popstate', update);
    return () => window.removeEventListener('popstate', update);
  }, []);
  return { ...resolveDashRoute(location.path), ...viewForPath(location.path), search: location.search };
}

function StandaloneDesk({ view, known }) {
  if (!known) {
    return <DashShell standalone view={view}>
      <div className="dash-body"><h1>Unknown dashboard</h1><p>No dashboard is registered at /dash/{view}</p><div className="dash-empty">Pick a dashboard
        <div className="dash-nav-links" style={{ marginTop: 10 }}>{Object.entries(DASH_ROUTES).map(([key, route]) => <a key={key} href={`/dash/${key}`}>{route.title}</a>)}</div>
      </div></div>
    </DashShell>;
  }
  if (view === 'cos' && new URLSearchParams(window.location.search).get('panel') === 'mailboxes') return <DashShell standalone view="cos"><MailboxBrowser api={api} /></DashShell>;
  if (view === 'cos' && new URLSearchParams(window.location.search).get('panel') !== 'conversation') return <DashShell standalone view="cos"><OwnerInbox api={api} /></DashShell>;
  if (view === 'tasks') return <DashShell standalone view="tasks"><TasksView api={api} /></DashShell>;
  if (view === 'projects') return <Projects standalone />;
  if (view === 'databases') return <DashShell standalone view="databases"><div className="dash-body">
    <div className="ops-heading"><div><div className="eyebrow">{DASH_ROUTES.databases.eyebrow}</div><h1>{DASH_ROUTES.databases.title}</h1><p>{DASH_ROUTES.databases.subtitle}</p></div></div>
    <ProjectInformation api={api} />
  </div></DashShell>;
  if (view === 'operations') return <OpsBoard standalone />;
  if (view === 'play') return <PlayWorkspace standalone />;
  return <ChannelStandalone key={view} channel={view} />;
}

function App() {
  const route = useDashRoute();
  const [theme, setTheme] = useState(storedTheme);
  const [themeError, setThemeError] = useState(false);
  function chooseTheme(value) {
    let selected = validTheme(value);
    try { localStorage.setItem('vp-theme', selected); setThemeError(false); }
    catch { selected = 'default'; setThemeError(true); }
    setTheme(selected);
  }
  useLayoutEffect(() => { document.documentElement.dataset.theme = theme; }, [theme]);
  const [authorized, setAuthorized] = useState(null);
  useEffect(() => { api('/session').then(() => setAuthorized(true)).catch(() => setAuthorized(false)); }, []);
  useEffect(() => { const expired = () => setAuthorized(false); window.addEventListener('vp-auth-expired', expired); return () => window.removeEventListener('vp-auth-expired', expired); }, []);
  const [updateAvailable, setUpdateAvailable] = useState(false);
  useEffect(() => {
    const loadedVersion = document.querySelector('script[type="module"]')?.getAttribute('src');
    const check = () => api('/version').then(data => { if (loadedVersion && data.version !== loadedVersion) setUpdateAvailable(true); }).catch(() => {});
    const timer = setInterval(check, 15000); window.addEventListener('focus', check);
    return () => { clearInterval(timer); window.removeEventListener('focus', check); };
  }, []);
  if (authorized === null) return <div className="splash">DUNKING SHEEP</div>;
  // Keep the owner on the conversations and boards in this desk.
  const content = authorized
    ? <Suspense fallback={<div className="splash" role="status">Loading dashboard…</div>}><StandaloneDesk key={`${route.view}:${route.search}`} view={route.view} known={route.known} /></Suspense>
    : <Login onSuccess={() => setAuthorized(true)} />;
  return <ThemeContext.Provider value={{ theme, chooseTheme, themeError }}>{updateAvailable && <div className="update-banner" role="status">A desk update is ready. Save any draft, then <button onClick={() => window.location.reload()}>Refresh now</button>.</div>}{content}</ThemeContext.Provider>;
}

createRoot(document.getElementById('root')).render(<App />);
