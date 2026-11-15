import React, { lazy, Suspense } from 'react';
import ChunkErrorBoundary from './ChunkErrorBoundary';
const ProjectTerminal = lazy(() => import('./ProjectTerminal'));
export default function LazyProjectTerminal(props) {
  return <ChunkErrorBoundary><Suspense fallback={<p role="status">Loading terminal…</p>}><ProjectTerminal {...props} /></Suspense></ChunkErrorBoundary>;
}
