// Only dimensions are sampled; no URL, user agent, identifier, or history.
export function startViewportReport(browser) {
  if (!browser.location.pathname.startsWith('/dash')) return;
  let timer, lastSent = -Infinity, lastSample = '';
  function report() {
    const { innerWidth, innerHeight, devicePixelRatio } = browser;
    const { width: screenWidth, height: screenHeight } = browser.screen;
    if (innerWidth < 1 || innerHeight < 1) return;
    const body = JSON.stringify({ innerWidth, innerHeight, devicePixelRatio, screenWidth, screenHeight });
    if (body === lastSample) return;
    lastSample = body;
    lastSent = browser.performance.now();
    browser.fetch('/api/viewport', {
      method: 'POST', credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json' }, body,
    }).catch(() => {}); // Telemetry must never interrupt the desk.
  }
  function resize() {
    browser.clearTimeout(timer);
    timer = browser.setTimeout(report, Math.max(300, 10000 - (browser.performance.now() - lastSent)));
  }
  report();
  browser.addEventListener('resize', resize);
  return () => { browser.clearTimeout(timer); browser.removeEventListener('resize', resize); };
}
if (typeof window !== 'undefined') startViewportReport(window);
