import test from 'node:test';
import assert from 'node:assert/strict';
import { startViewportReport } from './viewportReport.js';

function fixture(path = '/dash/cos') {
  let now = 0, id = 0;
  const timers = new Map(), handlers = new Map(), calls = [];
  const browser = {
    location: { pathname: path }, innerWidth: 1440, innerHeight: 900, devicePixelRatio: 1, screen: {width:1920, height:1080},
    performance: { now: () => now },
    fetch: (url, options) => { calls.push({ at: now, url, ...options }); return Promise.resolve({ ok: true }); },
    setTimeout: (fn, delay) => { timers.set(++id, { fn, at: now + delay }); return id; },
    clearTimeout: key => timers.delete(key),
    addEventListener: (name, fn) => handlers.set(name, fn),
    removeEventListener: name => handlers.delete(name),
  };
  return { browser, calls, resize: () => handlers.get('resize')?.(),
    advance(ms) {
      const end = now + ms;
      while ([...timers.values()].some(t => t.at <= end)) {
        const [key, timer] = [...timers].filter(([, t]) => t.at <= end).sort((a,b) => a[1].at-b[1].at)[0];
        now = timer.at; timers.delete(key); timer.fn();
      }
      now = end;
    }};
}
test('one load sample, debounced latest resize, at most once per ten seconds', () => {
  const f = fixture(), stop = startViewportReport(f.browser);
  assert.equal(f.calls.length, 1);
  assert.deepEqual(JSON.parse(f.calls[0].body), { innerWidth:1440, innerHeight:900, devicePixelRatio:1, screenWidth:1920, screenHeight:1080 });
  f.browser.innerWidth = 390; f.resize(); f.advance(5000);
  f.browser.innerWidth = 320; f.resize(); f.advance(4999);
  assert.equal(f.calls.length, 1); f.advance(1);
  assert.equal(f.calls.length, 2); assert.equal(JSON.parse(f.calls[1].body).innerWidth, 320);
  f.resize(); f.advance(10000); assert.equal(f.calls.length, 2); // unchanged
  f.browser.innerWidth = 390; f.resize(); f.advance(299); assert.equal(f.calls.length, 2);
  f.advance(1); assert.equal(f.calls.length, 3);
  stop(); f.browser.innerWidth = 1440; f.resize(); f.advance(10000); assert.equal(f.calls.length, 3);
});
test('login route and zero-sized viewports do not report', () => {
  const login=fixture('/'); startViewportReport(login.browser); assert.equal(login.calls.length,0);
  const f=fixture(); f.browser.innerWidth=0; startViewportReport(f.browser); assert.equal(f.calls.length,0);
});
