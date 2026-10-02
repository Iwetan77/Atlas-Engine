import {randomUUID} from 'node:crypto';

// A wallet request expires with its prepared transaction. No token can sign it on the server.
export function rawRequest({appId,walletId,message,hashFunction,expires}) {
  const params={bytes:message,encoding:'hex',hash_function:hashFunction};
  return {params,request:{version:1,method:'POST',url:`https://api.privy.io/v1/wallets/${walletId}/raw_sign`,
    body:{params},headers:{'privy-app-id':appId,'privy-request-expiry':String(expires)}}};
}
export class DeviceApprovals {
  #items=new Map();
  put({userId,wallet,appId,messages,hashFunction,data,operation,expires=Date.now()+180000}) {
    if(!messages.length || expires<=Date.now() || expires>Date.now()+180000)throw new Error('invalid preparation');
    for(const [id,item] of this.#items)if(item.expires<=Date.now())this.#items.delete(id);
    if(this.#items.size>=1000)throw new Error('approval queue full');
    const requests=messages.map(message=>rawRequest({appId,walletId:wallet.id,message,hashFunction,expires}));
    const prepareId=randomUUID();
    this.#items.set(prepareId,{operation,userId,walletId:wallet.id,address:wallet.address,requests,data,expires});
    return {prepareId,requests:requests.map(r=>r.request),expiresAtUnixMs:expires};
  }
  take({prepareId,userId,wallet,signatures,operation}) {
    const item=this.#items.get(prepareId);
    if(!item || item.operation!==operation || item.userId!==userId || item.walletId!==wallet.id || item.address!==wallet.address ||
      item.expires<=Date.now() || !Array.isArray(signatures) || signatures.length!==item.requests.length ||
      signatures.some(s=>typeof s!=='string'||!/^[A-Za-z0-9+/=_-]{40,200}$/.test(s)))throw new Error('approval changed, expired or used');
    this.#items.delete(prepareId);
    return {...item,signatures};
  }
}
export async function deviceSign(privy,item) {
  const signatures=[];
  for(let index=0;index<item.requests.length;index++){
    const {params}=item.requests[index];
    signatures.push((await privy.wallets().rawSign(item.walletId,{params,request_expiry:item.expires,
      authorization_context:{signatures:[item.signatures[index]]}})).signature);
  }
  return signatures;
}
