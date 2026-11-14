// Display-only identity: %1 is the VP agent. Keep server names and routing keys intact.
export function laneDisplayName(lane, fallback = 'Agent') {
  return lane?.pane_id === '%1' ? 'VP of Agentry' : lane?.name || fallback;
}

export function channelDisplayName(channel, fallback = 'the agent') {
  return channel?.id === 'cos' ? 'VP of Agentry' : laneDisplayName(channel, fallback);
}
