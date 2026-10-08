import webpush from 'web-push';

export function validSubscription(subscription) {
  if (!subscription || typeof subscription.endpoint !== 'string' || subscription.endpoint.length > 2048) return false;
  try {
    const url = new URL(subscription.endpoint);
    const host = url.hostname;
    const allowed = host === 'fcm.googleapis.com' || host === 'updates.push.services.mozilla.com' ||
      host === 'web.push.apple.com' || host.endsWith('.notify.windows.com');
    return allowed && url.protocol === 'https:' && !url.username && !url.password && !url.port &&
      /^[A-Za-z0-9_-]{87}$/.test(subscription.keys?.p256dh ?? '') &&
      /^[A-Za-z0-9_-]{22}$/.test(subscription.keys?.auth ?? '');
  } catch { return false; }
}
export function notificationKeys() { return webpush.generateVAPIDKeys(); }

// Lock-screen alerts contain no balances, amounts, asset names, recipients or transaction IDs.
export async function sendWebNotification({subscription, keys, noticeId}, send = webpush.sendNotification) {
  if (!validSubscription(subscription)) return {status:'expired'};
  if (!/^[a-f0-9]{32}$/.test(noticeId ?? '')) throw new Error('Invalid notification');
  const payload = {id:noticeId,title:'Atlas',body:'You have a new Atlas money update.',url:'/notifications'};
  try {
    await send(subscription, JSON.stringify(payload), {
      vapidDetails: {subject:'mailto:hello@justatlas.xyz', publicKey:keys.publicKey, privateKey:keys.privateKey},
      TTL:3600, urgency:'normal', timeout:10000,
    });
    return {status:'sent'};
  } catch (error) { return {status: [404,410].includes(error.statusCode) ? 'expired' : 'retry'}; }
}
