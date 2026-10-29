import { useLayoutEffect, useRef, useState } from 'react';

// Keep already-loaded messages when the polling window drops its oldest row.
export function mergeMessages(previous, incoming) {
  const rows = new Map(previous.map(row => [row.id, row]));
  incoming.forEach(row => rows.set(row.id, row));
  return [...rows.values()].sort((a, b) => a.id - b.id);
}

export function useMessageScroll(items, channel) {
  const ref = useRef(null);
  const position = useRef({ channel, follow: true, top: 0, ids: new Set(), loaded: false });
  const [hasNew, setHasNew] = useState(false);
  function jump() {
    const pane = ref.current;
    position.current.follow = true;
    if (pane) {
      pane.scrollTop = pane.scrollHeight;
      position.current.top = pane.scrollTop;
    }
    setHasNew(false);
  }
  function onScroll() {
    const pane = ref.current;
    if (!pane) return;
    const saved = position.current;
    const movedUp = pane.scrollTop < saved.top - 1;
    saved.follow = !movedUp && pane.scrollHeight - pane.scrollTop - pane.clientHeight <= 40;
    saved.top = pane.scrollTop;
    if (saved.follow) setHasNew(false);
  }
  useLayoutEffect(() => {
    const pane = ref.current;
    if (!pane) return;
    if (position.current.channel !== channel) {
      position.current = { channel, follow: true, top: 0, ids: new Set(), loaded: false };
      setHasNew(false);
    }
    const saved = position.current;
    const added = saved.loaded && items.some(item => !saved.ids.has(item.id));
    // follow/top were recorded by scroll events BEFORE this DOM update.
    if (saved.follow) pane.scrollTop = pane.scrollHeight;
    else {
      pane.scrollTop = saved.top;
      if (added) setHasNew(true);
    }
    saved.top = pane.scrollTop;
    saved.ids = new Set(items.map(item => item.id));
    saved.loaded = items.length > 0;
  }, [items, channel, hasNew]);
  return { ref, onScroll, hasNew, jump };
}
