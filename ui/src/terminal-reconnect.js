// One initial prompt retry; a snapshot alone cannot reset a failing stream.
export function terminalReconnect(now = () => performance.now()) {
  let attempt = 0, openedAt = null, receivedScreen = false;
  return {
    get retrying() { return attempt > 0; },
    opened() { openedAt = now(); receivedScreen = false; },
    received() { receivedScreen = true; },
    closed() {
      // The server sends a snapshot on every connection, including ones that fail
      // immediately afterwards. Require two seconds of uptime as well as data.
      if (receivedScreen && openedAt !== null && now() - openedAt >= 2000) attempt = 0;
      openedAt = null;
      receivedScreen = false;
      const retry = attempt++;
      return retry === 0 ? 0 : Math.min(1000 * (2 ** Math.min(retry - 1, 4)), 15000);
    },
  };
}
