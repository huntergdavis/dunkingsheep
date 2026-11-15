import React, { useEffect, useState } from 'react';
import { useMessageScroll, mergeMessages } from './useMessageScroll';
import { MessageList } from '@chatscope/chat-ui-kit-react';
import { channelDisplayName } from './lane-label.js';
import { channelDeliveryLabel } from './delivery-label.js';
import { api, notifyNote, DashShell } from './DeskShared';
import { Composer, MessageCard } from './ConversationWidgets';

function ChannelStandalone({ channel }) {
  const [channels, setChannels] = useState([]);
  const [items, setItems] = useState([]);
  const [pending, setPending] = useState([]);
  const [error, setError] = useState('');
  const [updated, setUpdated] = useState(null);
  const [lastDelivery, setLastDelivery] = useState(null);
  const messageScroll = useMessageScroll(items, channel);
  const selected = channels.find(c => c.id === channel);
  useEffect(() => {
    let live = true;
    setLastDelivery(null);
    const load = async () => {
      try {
        const [roster, messages] = await Promise.all([api('/channels'), api(channel === 'cos' ? '/owner-thread?limit=50' : `/messages?channel=${encodeURIComponent(channel)}&limit=50`)]);
        if (!live) return;
        setChannels(roster.channels || []);
        setItems(old => mergeMessages(old, messages.items || []));
        setLastDelivery(messages.last_delivery || null);
        setUpdated(new Date());
        setError('');
      } catch (e) { if (live) setError(e.message); }
    };
    load();
    const timer = setInterval(load, 5000);
    return () => { live = false; clearInterval(timer); };
  }, [channel]);
  async function send(data) {
    if (channel === 'cos') {
      // The Front Desk is reached through the owner mailbox (t1151): the row is stored
      // first, then the thread re-reads it, so what shows is the daemon's record.
      const receipt = await api('/owner-send', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ text: data.body, attachment_ids: data.attachment_ids }) });
      const thread = await api('/owner-thread?limit=50');
      messageScroll.jump();
      setItems(old => mergeMessages(old, thread.items || []));
      { const note = notifyNote(receipt.notify); if (note) setError(`Stored as ${receipt.message_id}.${note}`); }
      return;
    }
    const item = await api('/pane-send', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ pane: selected.pane_id, channel, text: data.body, attachment_ids: data.attachment_ids }) });
    messageScroll.jump();
    setItems(old => mergeMessages(old, [item]));
  }
  return <DashShell standalone view={channel} side={<>
      {selected && <span className="dash-meta">{channelDisplayName(selected)} · {selected.status}</span>}
      <span className="dash-meta">updated {updated ? updated.toLocaleTimeString() : '—'}</span>
    </>}>
    <div className="dash-body dash-conversation">
      {channel === 'cos' && <a href="/dash/cos">Back to Inbox</a>}
      {error && <div className="dash-error" role="alert">{error}</div>}
      <div className="channel-delivery-summary" role="status" title="Time from the desk accepting your message to the agent harness picking it up; this does not mean a reply is ready.">
        {channelDeliveryLabel(lastDelivery)}
      </div>
      <div className="conversation" ref={messageScroll.ref} onScroll={messageScroll.onScroll}>
        <div className="conversation-inner">
          {!items.length && <div className="empty-state"><div className="empty-icon">✦</div><h2>Direct line to {channelDisplayName(selected)}</h2></div>}
          <MessageList autoScrollToBottom={false} autoScrollToBottomOnMount={false}>{items.map(item => <MessageCard key={item.id} item={item} frontDesk={channel === 'cos'} onSendNow={() => {}} onInspect={() => {
            const pane = selected?.pane_id;
            window.location.assign(pane ? `/dash/projects?pane=${encodeURIComponent(pane)}` : '/dash/projects');
          }} />)}</MessageList>
        </div>
      </div>
      {messageScroll.hasNew && <button className="chat-new-messages" onClick={messageScroll.jump}>New messages · Jump to latest</button>}
      <Composer channel={channel} onSend={send} pending={pending} setPending={setPending} disabled={!selected?.can_message} />
    </div>
  </DashShell>;
}
export default ChannelStandalone;
