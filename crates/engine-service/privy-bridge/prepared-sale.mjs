import {createHash, randomUUID} from 'node:crypto';
const fingerprint = jwt => createHash('sha256').update(jwt).digest('hex');
const fields = ['userId','walletId','address','quoteId','coinType','amount','minimumOut','expiresAtUnixMs'];
export class PreparedSales {
  #items = new Map();
  put(scope, jwt, data, now = Date.now()) {
    if (!jwt || fields.some(k => scope[k] == null) || !Number.isSafeInteger(scope.expiresAtUnixMs) ||
        scope.expiresAtUnixMs <= now || scope.expiresAtUnixMs > now + 180_000 ||
        !/^[1-9][0-9]*$/.test(scope.amount) || !/^[1-9][0-9]*$/.test(scope.minimumOut)) {
      throw new Error('invalid sale scope');
    }
    for (const [id, item] of this.#items) if (item.scope.expiresAtUnixMs <= now) this.#items.delete(id);
    if (this.#items.size >= 1000) throw new Error('sale queue full');
    const id = randomUUID();
    this.#items.set(id,{scope:Object.freeze(Object.fromEntries(fields.map(k => [k,scope[k]]))),jwt:fingerprint(jwt),data});
    return id;
  }
  take(id, scope, jwt, now = Date.now()) {
    const item = this.#items.get(id);
    if (!item || item.scope.expiresAtUnixMs <= now || !jwt || item.jwt !== fingerprint(jwt) ||
        fields.some(k => item.scope[k] !== scope[k])) throw new Error('sale changed, expired or used');
    this.#items.delete(id);
    return item;
  }
}

// The signer fetches the confirmed intent itself. The public bridge request cannot supply a coin,
// amount, output floor or recipient. The engine consumes this permission atomically in Postgres.
export async function salePermission({intentId,userId,wallet,accessToken}) {
  if (!/^near-intent-[0-9]+-[0-9]+$/.test(intentId ?? '')) throw new Error('invalid sale intent');
  const port = Number((process.env.ATLAS_BALANCE_BIND ?? '127.0.0.1:3000').split(':').at(-1));
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('invalid engine port');
  const headers = {authorization:`Bearer ${accessToken}`};
  const res = await fetch(`http://127.0.0.1:${port}/v1/intents/${intentId}/sale-permission`, {
    method:'POST',headers,signal:AbortSignal.timeout(20_000)});
  if (!res.ok) throw new Error('sale permission unavailable or used');
  const scope = await res.json();
  if (scope.userId !== userId || scope.address !== wallet.address || scope.quoteId !== intentId) {
    throw new Error('sale identity mismatch');
  }
  return {...scope,walletId:wallet.id};
}
