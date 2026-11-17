import React, { useState } from 'react';
export default function TaskDecision({ api, task }) {
  const [question,setQuestion] = useState(null), [reply,setReply] = useState(''), [busy,setBusy] = useState(false), [error,setError] = useState(''), [sent,setSent] = useState(false);
  async function act(path, extra = {}) {
    setBusy(true); setError('');
    try { const data = await api(`/tasks/${path}`, {method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify({task_id:task.id,...extra})}); if(path === 'question') setQuestion(data); else setSent(true); }
    catch(e) { setError(e.message); }
    finally { setBusy(false); }
  }
  return <section aria-label={`Decision for ${task.id}`}>
    {error && <p role="alert">{error}</p>}
    {sent ? <p role="status">Decision recorded. Refresh tasks to see the current state.</p> : <>
      {!question ? <button disabled={busy} onClick={() => act('question')}>Read question & answer</button> : <>
        <p>{question.question}</p>
        {question.options?.length ? question.options.map(option => <button key={option.number} disabled={busy} onClick={() => act('answer',{version:question.version,option_number:option.number})}>{option.label}</button>) : <><input aria-label={`Answer ${task.id}`} value={reply} onChange={e => setReply(e.target.value)} /><button disabled={busy || !reply.trim()} onClick={() => act('answer',{version:question.version,reply})}>Answer</button></>}
        <button disabled={busy} onClick={() => act('not-now')}>Not now</button>
      </>}
    </>}
  </section>;
}
