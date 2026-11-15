import React, { createContext, useContext } from 'react';
import { formatTimestamp, timestampIso } from './timestamps.js';

export const ServerTimeZone = createContext('UTC');
export const useServerTimeZone = () => useContext(ServerTimeZone);

export default function Timestamp({ value, seconds = false }) {
  const zone = useServerTimeZone();
  const iso = timestampIso(value, seconds);
  const date = typeof value === 'string' && /^\d{4}-\d{2}-\d{2}$/.test(value) ? value : null;
  return <time dateTime={iso || date || undefined} title={iso || date || undefined}>{formatTimestamp(value, zone, seconds)}</time>;
}
