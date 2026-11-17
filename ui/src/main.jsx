import { ServerTimeZone } from './Timestamp';
import './viewportReport';
import React, { useEffect, useLayoutEffect, useState, lazy, Suspense } from 'react';
import { createRoot } from 'react-dom/client';
import '@chatscope/chat-ui-kit-styles/dist/default/styles.min.css';
import './style.css';
import './dash.css';
import './scrollbars.css';
import { canonicalDashLocation, DASH_ROUTES, resolveDashRoute, viewForPath } from './dash-route.js';
import { ThemeContext, validTheme, storedTheme, api, DashShell, Login } from './DeskShared';
import ChunkErrorBoundary from './ChunkErrorBoundary';
const OwnerInbox = lazy(() => import('./OwnerInbox'));
const MailboxBrowser = lazy(() => import('./MailboxBrowser'));
const TasksView = lazy(() => import('./TasksView'));
const ProjectInformation = lazy(() => import('./ProjectInformation'));
const Projects = lazy(() => import('./ProjectsView'));
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
  if (view === 'cos' && new URLSearchParams(window.location.search).get('panel') === 'webmail') return <DashShell standalone view="cos"><p><a href="/dash/cos">Back to Inbox</a></p><iframe title="Dunking Sheep mail" src="/webmail/en/mail/" style={{width:'100%', height:'80vh', border:0}} /></DashShell>;
  if (view === 'cos' && new URLSearchParams(window.location.search).get('panel') !== 'conversation') return <DashShell standalone view="cos"><OwnerInbox api={api} /></DashShell>;
  if (view === 'tasks') return <DashShell standalone view="tasks"><TasksView api={api} /></DashShell>;
  if (view === 'projects') return <Projects standalone />;
  if (view === 'knowledge') return <DashShell standalone view="knowledge"><div className="dash-body">
    <div className="ops-heading"><div><div className="eyebrow">{DASH_ROUTES.knowledge.eyebrow}</div><h1>{DASH_ROUTES.knowledge.title}</h1><p>{DASH_ROUTES.knowledge.subtitle}</p></div></div>
    <ProjectInformation api={api} />
  </div></DashShell>;
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
  const [timeZone, setTimeZone] = useState(null);
  const loadSession = () => api('/session').then(data => { setTimeZone(data.time_zone || null); setAuthorized(true); }).catch(() => setAuthorized(false));
  useEffect(() => { loadSession(); }, []);
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
    ? <ChunkErrorBoundary key={`${route.view}:${route.search}`}><Suspense fallback={<div className="splash" role="status">Loading dashboard…</div>}><StandaloneDesk view={route.view} known={route.known} /></Suspense></ChunkErrorBoundary>
    : <Login onSuccess={loadSession} />;
  return <ServerTimeZone.Provider value={timeZone}><ThemeContext.Provider value={{ theme, chooseTheme, themeError }}>{updateAvailable && <div className="update-banner" role="status">A desk update is ready. Save any draft, then <button onClick={() => window.location.reload()}>Refresh now</button>.</div>}{content}</ThemeContext.Provider></ServerTimeZone.Provider>;
}

createRoot(document.getElementById('root')).render(<App />);
