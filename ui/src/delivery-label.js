// A stored push is not delivered. Only the server's matched pickup receipt
// supplies the elapsed time; browser polling time is never a latency sample.
export function deliveryDuration(item) {
  if (item?.delivery !== 'delivered' || !Number.isFinite(item.delivered_at) || item.delivered_at <= 0 ||
      !Number.isFinite(item.delivery_latency_s) || item.delivery_latency_s < 0) return null;
  const seconds = item.delivery_latency_s;
  return seconds > 0 && seconds < 0.1 ? '<0.1s' : `${seconds.toFixed(1)}s`;
}

export function messageDeliveryLabel(item) {
  const duration = deliveryDuration(item);
  if (duration !== null) return `delivered in ${duration}`;
  if (item.delivery === 'delivered') return 'delivered · timing unavailable';
  return item.outcome || { inbox: 'delivered to inbox', read: 'read', acked: 'handled' }[item.delivery] || item.delivery;
}

export function channelDeliveryLabel(item) {
  const duration = deliveryDuration(item);
  if (duration !== null) return `Last delivered in ${duration}`;
  return item?.delivery === 'delivered' ? 'Last delivered · timing unavailable' : 'No confirmed delivery time yet';
}
