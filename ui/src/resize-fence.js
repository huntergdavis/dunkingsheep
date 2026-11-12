// Shared by all tabs on the origin. Unknown outcomes retain the marker.
export async function fencedResize({ locks, storage, key, send }) {
  if (!locks) throw new Error('Cross-tab resize coordination unavailable');
  return locks.request(key, { ifAvailable: true }, async lock => {
    if (!lock) return 'busy';
    if (storage.getItem(key) !== null) return 'unconfirmed';
    storage.setItem(key, 'unconfirmed');
    const response = await send();
    if ([400, 401, 403, 404, 409].includes(response.status)) {
      storage.removeItem(key);
      return 'refused';
    }
    if (!response.ok) throw new Error(`Resize unavailable (${response.status})`);
    storage.removeItem(key);
    return 'applied';
  });
}
