import React, { lazy, Suspense } from 'react';
const ProjectTerminal = lazy(() => import('./ProjectTerminal'));
export default function LazyProjectTerminal(props) {
  return <Suspense fallback={<p role="status">Loading terminal…</p>}><ProjectTerminal {...props} /></Suspense>;
}
