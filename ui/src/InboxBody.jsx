import React, { lazy, Suspense, useState } from 'react';
const MessageBody = lazy(() => import('./MessageBody'));
// Plain text is immediately available. Rich rendering is an explicit, deferred cost.
export default function InboxBody({ body }) {
  const [formatted, setFormatted] = useState(false);
  const plain = <pre className="inbox-plain-body">{body}</pre>;
  return <div className="inbox-body">
    <button type="button" aria-pressed={formatted} onClick={() => setFormatted(value => !value)}>{formatted ? 'Plain text' : 'Format text'}</button>
    {formatted ? <Suspense fallback={plain}><MessageBody body={body} frontDesk /></Suspense> : plain}
  </div>;
}
