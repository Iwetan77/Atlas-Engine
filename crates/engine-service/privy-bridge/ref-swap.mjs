// Ref contract views and NEAR transaction encoding. All amounts stay integers.
import {randomUUID} from 'node:crypto';
import {sha256} from '@noble/hashes/sha256';
import {ed25519} from '@noble/curves/ed25519';
import {PreparedSales,salePermission} from './prepared-sale.mjs';
import {fromBase58, toBase58} from '@mysten/sui/utils';
export const REF = 'v2.ref-finance.near';
export const WRAP = 'wrap.near';
const RPC = process.env.ATLAS_NEAR_MAINNET_RPC_URL ?? 'https://rpc.mainnet.near.org';
export const GAS_RESERVE = 50000000000000000000000n; // 0.05 NEAR, including storage.
export function account(value) {
  if (typeof value !== 'string' || value.length < 2 || value.length > 64 ||
      !/^[a-z0-9]+(?:[._-][a-z0-9]+)*$/.test(value)) throw new Error('invalid token account');
  return value;
}
export async function rpc(method, params) {
  for(const endpoint of [RPC,'https://free.rpc.fastnear.com']) {
    try {
      const response = await fetch(endpoint,{method:'POST',headers:{'content-type':'application/json'},
        body:JSON.stringify({jsonrpc:'2.0',id:'atlas',method,params}),signal:AbortSignal.timeout(12000)});
      if(!response.ok)continue;
      const body=await response.json();
      if(body.error)continue;
      return body.result;
    } catch {}
  }
  throw new Error('contract view unavailable: '+(params.method_name??method));
}
export async function view(contract, method, args={}) {
  const result=await rpc('query',{request_type:'call_function',finality:'final',account_id:account(contract),
    method_name:method,args_base64:Buffer.from(JSON.stringify(args)).toString('base64')});
  return JSON.parse(Buffer.from(result.result).toString());
}
let poolCache;
export async function pools() {
  if(poolCache && Date.now()-poolCache.at<300000)return poolCache.value;
  // The indexer sometimes refuses stale blocks. Fall back to contract views, not stale quotes.
  let value;
  try {
    const r=await fetch('https://indexer.ref.finance/fetchAllPools',{signal:AbortSignal.timeout(10000)});
    const body=await r.json();
    if(Array.isArray(body.simplePools))value=body.simplePools;
  } catch {}
  if(!value){
    const total=Number(await view(REF,'get_number_of_pools'));
    if(!Number.isSafeInteger(total)||total>50000)throw new Error('pool list unavailable');
    value=[];
    for(let start=0;start<total;start+=2000){
      const starts=[start,start+500,start+1000,start+1500].filter(n=>n<total);
      const pages=await Promise.all(starts.map(async first=>{
        const page=await view(REF,'get_pools',{from_index:first,limit:500});
        return page.map((p,i)=>({...p,id:first+i})).filter(p=>p.pool_kind==='SIMPLE_POOL');
      }));
      value.push(...pages.flat());
    }
  }
  poolCache={at:Date.now(),value};return value;
}
export function directPools(list,token,nearPrice){
  account(token);
  return list.filter(p=>p.pool_kind==='SIMPLE_POOL' && p.token_account_ids?.length===2 &&
    p.token_account_ids.includes(token) && p.token_account_ids.includes(WRAP) &&
    Number(p.amounts[p.token_account_ids.indexOf(WRAP)])/1e24*nearPrice>=2500);
}
export async function tokenInfo(token){
  account(token);const metadata=await view(token,'ft_metadata');
  if(!Number.isInteger(metadata.decimals)||metadata.decimals<0||metadata.decimals>24||
      typeof metadata.symbol!=='string'||typeof metadata.name!=='string')throw new Error('token details unavailable');
  return metadata;
}
let priceCache;
export async function prices(){
  if(priceCache && Date.now()-priceCache.at<60000)return priceCache.value;
  const r=await fetch('https://indexer.ref.finance/list-token-price',{signal:AbortSignal.timeout(10000)});
  if(!r.ok)throw new Error('prices unavailable');
  priceCache={at:Date.now(),value:await r.json()};return priceCache.value;
}
export async function quoteRef(token,amount,sell=false){
  if(BigInt(amount)<=0n)throw new Error('invalid amount');
  const priceMap=await prices();const nearPrice=Number(priceMap[WRAP]?.price);
  if(!Number.isFinite(nearPrice)||nearPrice<=0)throw new Error('price unavailable');
  const candidates=directPools(await pools(),token,nearPrice);
  let best;
  for(const pool of candidates){
    const input=sell?token:WRAP,output=sell?WRAP:token;
    const out=BigInt(await view(REF,'get_return',{pool_id:Number(pool.id),token_in:input,amount_in:String(amount),token_out:output}));
    if(out>0n&&(!best||out>BigInt(best.amountOut)))best={poolId:Number(pool.id),tokenIn:input,tokenOut:output,
      amountIn:String(amount),amountOut:String(out),minimumOut:String(out*99n/100n)};
  }
  if(!best)throw new Error('not enough liquidity to trade this token');
  return best;
}
function uint(value,bytes){
  let n=BigInt(value);if(n<0n||n>=1n<<BigInt(bytes*8))throw new Error('integer outside range');
  const b=Buffer.alloc(bytes);for(let i=0;i<bytes;i++){b[i]=Number(n&255n);n>>=8n;}return b;
}
function bytes(value){return Buffer.concat([uint(value.length,4),Buffer.from(value)]);}
function string(value){return bytes(Buffer.from(value));}
export function call(method,args,gas,deposit){return {method,args,gas:String(gas),deposit:String(deposit)};}
// Transaction V0: account, ed25519 key, nonce, receiver, recent hash and FunctionCall actions.
export function transaction({signer,publicKey,nonce,receiver,blockHash,actions}){
  account(signer);account(receiver);
  const key=Buffer.from(fromBase58(publicKey.replace(/^ed25519:/,'')));
  const hash=Buffer.from(fromBase58(blockHash));
  if(key.length!==32||hash.length!==32||!actions.length||actions.length>4)throw new Error('invalid transaction');
  return Buffer.concat([string(signer),Buffer.from([0]),key,uint(nonce,8),string(receiver),hash,uint(actions.length,4),
    ...actions.map(a=>Buffer.concat([Buffer.from([2]),string(a.method),bytes(Buffer.from(JSON.stringify(a.args))),uint(a.gas,8),uint(a.deposit,16)]))]);
}
export function swapCall(q){
  account(q.tokenIn);account(q.tokenOut);
  if(!Number.isSafeInteger(q.poolId)||q.poolId<0||BigInt(q.amountIn)<=0n||BigInt(q.minimumOut)<=0n)throw new Error('invalid quote');
  return call('ft_transfer_call',{receiver_id:REF,amount:q.amountIn,msg:JSON.stringify({force:0,actions:[{
    pool_id:q.poolId,token_in:q.tokenIn,token_out:q.tokenOut,amount_in:q.amountIn,min_amount_out:q.minimumOut}]})},180000000000000n,1n);
}
export function transactionId(bytes,sha256){return toBase58(sha256(bytes));}
// The nonce after both what the chain reports and what this run already used.
export function nextNonce(onChain,chain){
  const fromChain=BigInt(onChain)+1n;
  const nonce=chain.next!==undefined&&chain.next>fromChain?chain.next:fromChain;
  chain.next=nonce+1n;
  return nonce;
}

const prepared = new PreparedSales();
const active = new Set();
function publicKey(wallet){
  // Privy's NEAR wallets are implicit accounts: the address is the ed25519 public key in hex.
  if(!/^[0-9a-f]{64}$/.test(wallet.address))throw new Error('unsupported wallet address');
  return Buffer.from(wallet.address,'hex');
}
// Transactions sent one after another (storage, wrap, swap) share `chain`, so each takes the next
// nonce even when the node hasn't caught up with the last one yet.
async function sendCall(wallet,receiver,actions,rawSign,expires,chain={}){
  if(Date.now()>=expires)throw new Error('swap expired');
  const key=publicKey(wallet),publicKeyString='ed25519:'+toBase58(key);
  const access=await rpc('query',{request_type:'view_access_key',finality:'optimistic',account_id:wallet.address,public_key:publicKeyString});
  if(!Number.isSafeInteger(access.nonce))throw new Error('nonce unavailable');
  const nonce=nextNonce(access.nonce,chain);
  const bytes=transaction({signer:wallet.address,publicKey:publicKeyString,nonce,
    receiver,blockHash:access.block_hash,actions});
  if(Date.now()>=expires)throw new Error('swap expired');
  const signature=Buffer.from((await rawSign('0x'+bytes.toString('hex'))).replace(/^0x/,''),'hex');
  if(signature.length!==64||!ed25519.verify(signature,sha256(bytes),key))throw new Error('signature did not verify');
  const signed=Buffer.concat([bytes,Buffer.from([0]),signature]).toString('base64');
  // Never retry submission: a lost response is ambiguous, and this preparation is already consumed.
  const response=await fetch(RPC,{method:'POST',headers:{'content-type':'application/json'},
    body:JSON.stringify({jsonrpc:'2.0',id:'atlas',method:'broadcast_tx_commit',params:[signed]}),signal:AbortSignal.timeout(45000)});
  const result=await response.json();
  if(!response.ok||result.error||result.result?.status?.Failure)throw new Error('transaction outcome unavailable');
  return result.result;
}
async function storage(wallet,token,rawSign,expires,chain){
  if(await view(token,'storage_balance_of',{account_id:wallet.address}))return;
  const bounds=await view(token,'storage_balance_bounds');
  const minimum=BigInt(bounds.min);
  if(minimum>10000000000000000000000n)throw new Error('token storage cost is too high');
  await sendCall(wallet,token,[call('storage_deposit',{account_id:wallet.address,registration_only:true},30000000000000n,minimum)],rawSign,expires,chain);
}
export function receivedToken(result,token,owner){
  let amount=0n;
  for(const receipt of result.receipts_outcome??[]){
    if(receipt.outcome?.executor_id!==token||receipt.outcome.status?.Failure)continue;
    for(const log of receipt.outcome.logs??[]){
      if(!log.startsWith('EVENT_JSON:'))continue;
      let event;try{event=JSON.parse(log.slice(11));}catch{continue;}
      if(event.standard!=='nep141'||event.event!=='ft_transfer')continue;
      for(const item of event.data??[])if(item.old_owner_id===REF&&item.new_owner_id===owner)amount+=BigInt(item.amount);
    }
  }
  return amount;
}
export async function prepareRef({intentId,wallet,userId,userJwt,accessToken,identityToken}){
  const scope=await salePermission({intentId,wallet,userId,userJwt,accessToken,identityToken});
  if(scope.network!=='near'||!['buy','sell'].includes(scope.side))throw new Error('invalid swap permission');
  const sell=scope.side==='sell';account(scope.coinType);
  const balance=BigInt((await rpc('query',{request_type:'view_account',finality:'final',account_id:wallet.address})).amount);
  if(balance<GAS_RESERVE)throw new Error('not enough for the network fee');
  const held=BigInt(await view(sell?scope.coinType:WRAP,'ft_balance_of',{account_id:wallet.address}));
  if(sell?held<BigInt(scope.amount):held+balance-GAS_RESERVE<BigInt(scope.amount))throw new Error('holding changed');
  const q=await quoteRef(scope.coinType,scope.amount,sell);
  if(BigInt(q.minimumOut)<BigInt(scope.minimumOut))throw new Error('price changed');
  // Use the confirmed floor, never a later lower minimum.
  q.minimumOut=scope.minimumOut;
  return {saleId:prepared.put(scope,userJwt,{q,sell}),scope};
}
export async function commitRef({saleId,scope,wallet,userId,userJwt,rawSign}){
  if(scope.userId!==userId||scope.walletId!==wallet.id||scope.address!==wallet.address)throw new Error('wallet changed');
  if(active.has(wallet.id))throw new Error('another swap is settling');
  const item=prepared.take(saleId,scope,userJwt);active.add(wallet.id);
  try{
    const {q,sell}=item.data,expires=item.scope.expiresAtUnixMs,chain={};
    await storage(wallet,q.tokenOut,rawSign,expires,chain);
    if(!sell){
      await storage(wallet,WRAP,rawSign,expires,chain);
      const held=BigInt(await view(WRAP,'ft_balance_of',{account_id:wallet.address}));
      const wrap=BigInt(q.amountIn)>held?BigInt(q.amountIn)-held:0n;
      if(wrap>0n){
        const native=BigInt((await rpc('query',{request_type:'view_account',finality:'final',account_id:wallet.address})).amount);
        if(native<wrap+GAS_RESERVE)throw new Error('not enough for the network fee');
        await sendCall(wallet,WRAP,[call('near_deposit',{},30000000000000n,wrap)],rawSign,expires,chain);
      }
    }
    const held=BigInt(await view(q.tokenIn,'ft_balance_of',{account_id:wallet.address}));
    if(held<BigInt(q.amountIn))throw new Error('holding changed');
    const result=await sendCall(wallet,q.tokenIn,[swapCall(q)],rawSign,expires,chain);
    const amount=receivedToken(result,q.tokenOut,wallet.address);
    if(amount<BigInt(q.minimumOut))throw new Error('swap output could not be verified');
    // Keep proceeds wrapped: 1Click's INTENTS deposit accepts wNEAR directly.
    return {ok:true,digest:result.transaction.hash,amountOut:amount.toString()};
  } finally {active.delete(wallet.id);}
}
let listCache;
async function tokenList(){
  if(listCache && Date.now()-listCache.at<300000)return listCache.value;
  const r=await fetch('https://indexer.ref.finance/list-token',{signal:AbortSignal.timeout(10000)});
  if(!r.ok)throw new Error('token list unavailable');
  listCache={at:Date.now(),value:await r.json()};return listCache.value;
}
export async function searchRef(query){
  if(typeof query!=='string'||query.length<2||query.length>64)return [];
  const priceMap=await prices();
  const catalog=await tokenList();
  const candidates=query.endsWith('.near')?[query]:Object.keys(catalog).filter(k=>
    [catalog[k]?.symbol,catalog[k]?.name].some(v=>typeof v==='string'&&v.toLowerCase().includes(query.toLowerCase()))).slice(0,5);
  const list=await pools(),nearPrice=Number(priceMap[WRAP]?.price);
  const found=[];
  for(const token of candidates){
    try{
      const metadata=await tokenInfo(token),pairs=directPools(list,token,nearPrice);
      let price=Number(priceMap[token]?.price??0);
      if(!price&&pairs.length){
        const pool=pairs[0],i=pool.token_account_ids.indexOf(token),j=1-i;
        price=Number(pool.amounts[j])/1e24*nearPrice/(Number(pool.amounts[i])/10**metadata.decimals);
      }
      found.push({token,...metadata,price:String(price),tradeable:pairs.length>0&&Number.isFinite(price)&&price>0});
    }catch{}
  }
  return found;
}

const cashouts=new PreparedSales();
export async function prepareCashout({wallet,userId,userJwt,evmWallet,solanaWallet,amount,minimumOut}){
  if(!solanaWallet&&!/^0x[0-9a-fA-F]{40}$/.test(evmWallet))throw new Error('cash wallet unavailable');
  if(BigInt(amount)<=0n||BigInt(minimumOut)<=0n)throw new Error('invalid cashout');
  if(BigInt(await view(WRAP,'ft_balance_of',{account_id:wallet.address}))<BigInt(amount))throw new Error('holding changed');
  const request={dry:false,swapType:'EXACT_INPUT',slippageTolerance:100,originAsset:'nep141:wrap.near',depositType:'INTENTS',
    destinationAsset:solanaWallet?'nep141:sol-5ce3bf3a31af18be40ba30f721101b4341690186.omft.near':'nep141:base-0x833589fcd6edb6e08f4c7c32d4f71b54bda02913.omft.near',
    amount:String(amount),recipient:solanaWallet??evmWallet,recipientType:'DESTINATION_CHAIN',refundTo:wallet.address,refundType:'ORIGIN_CHAIN',deadline:new Date(Date.now()+240000).toISOString()};
  const response=await fetch('https://1click.chaindefuser.com/v0/quote',{method:'POST',headers:{'content-type':'application/json',...(process.env.NEAR_INTENTS_API_KEY?{'X-API-Key':process.env.NEAR_INTENTS_API_KEY}:{})},body:JSON.stringify(request),signal:AbortSignal.timeout(20000)});
  if(!response.ok)throw new Error('cashout route unavailable');
  const q=(await response.json()).quote;
  if(q.amountIn!==String(amount)||BigInt(q.minAmountOut??q.amountOut??0)<BigInt(minimumOut)||
    !/^[a-zA-Z0-9._-]{2,128}$/.test(q.depositAddress??'')||q.depositMemo!=null)throw new Error('cashout route changed');
  const scope={userId,walletId:wallet.id,address:wallet.address,quoteId:randomUUID(),coinType:WRAP,amount:String(amount),minimumOut:String(minimumOut),expiresAtUnixMs:Date.now()+180000};
  const cashoutId=cashouts.put(scope,userJwt,{depositAddress:q.depositAddress});
  return {cashoutId,scope,depositAddress:q.depositAddress,amountOut:q.amountOut};
}
export async function commitCashout({cashoutId,scope,wallet,userId,userJwt,rawSign}){
  if(scope.userId!==userId||scope.walletId!==wallet.id||scope.address!==wallet.address)throw new Error('wallet changed');
  const item=cashouts.take(cashoutId,scope,userJwt);
  const held=BigInt(await view(WRAP,'ft_balance_of',{account_id:wallet.address}));
  const native=BigInt((await rpc('query',{request_type:'view_account',finality:'final',account_id:wallet.address})).amount);
  if(held<BigInt(item.scope.amount)||native<GAS_RESERVE)throw new Error('holding changed');
  const result=await sendCall(wallet,WRAP,[call('ft_transfer_call',{receiver_id:'intents.near',amount:item.scope.amount,msg:item.data.depositAddress},100000000000000n,1n)],rawSign,item.scope.expiresAtUnixMs);
  let used;try{used=JSON.parse(Buffer.from(result.status.SuccessValue,'base64').toString());}catch{}
  return {ok:used===item.scope.amount,digest:result.transaction.hash};
}
