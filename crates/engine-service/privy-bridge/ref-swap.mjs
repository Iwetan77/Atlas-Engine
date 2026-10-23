// Ref contract views and NEAR transaction encoding. All amounts stay integers.
import {randomUUID} from 'node:crypto';
import {sha256} from '@noble/hashes/sha256';
import {ed25519} from '@noble/curves/ed25519';
import {PreparedSales,salePermission} from './prepared-sale.mjs';
import {fromBase58, toBase58} from '@mysten/sui/utils';
export const REF = 'v2.ref-finance.near';
export const WRAP = 'wrap.near';
// rpc.mainnet.near.org is deprecated and refuses requests: FastNEAR's public endpoints instead.
const RPC = process.env.ATLAS_NEAR_MAINNET_RPC_URL ?? 'https://free.rpc.fastnear.com';
export const GAS_RESERVE = 50000000000000000000000n; // 0.05 NEAR, including storage (Ref swaps).
// What a Ref sale needs in the wallet (the engine's NEAR_REF_SALE_GAS, which it tops up to from cash):
// the swap's 180 Tgas, mostly refunded, token storage, then the cash-out's 0.01.
export const REF_SALE_GAS = 30000000000000000000000n; // 0.03 NEAR
// A sale straight to cash: one transaction (register the wallet on wNEAR the first time, wrap, register
// 1Click's fresh deposit address, transfer). Before running it NEAR holds back its prepaid gas at
// least at min_gas_purchase_price (0.001 NEAR per Tgas since protocol 85, ten times what gas costs; the
// difference comes back after), and the account must still keep its own storage. 100 Tgas of prepaid gas
// meant 0.1 NEAR held back, so Max sales of NEAR were refused (NotEnoughBalance). Each call now prepays
// what it uses with room to spare (wNEAR's registration burns about 1.6 Tgas, wrap and transfer about
// 4.1 together, live 2026-10-23), and the room follows NEAR's live prices. The engine's sale_room is the
// same formula.
export const STORAGE_GAS = 3000000000000n, WRAP_GAS = 3000000000000n;
export const transferGas = token => token===WRAP ? 5000000000000n : 10000000000000n;
const SALE_PREPAID_GAS = 14000000000000n; // the most a sale prepays: 3 + 3 + 3 + 5 Tgas (3 + 10 for other coins)
const SALE_EXEC_GAS = 4000000000000n; // the receipt's and calls' own execution fees (about 3.3 Tgas)
const SALE_SEND_GAS = 1000000000000n; // converting the transaction (about 0.9 Tgas), at the gas price
const REGISTRATION_BYTES = 250n; // two wNEAR registrations: the wallet's and 1Click's (125 bytes each)
export function saleRoom(view,{gasPrice,perByte,purchasePrice}) {
  if(!Number.isSafeInteger(view?.storage_usage))throw new Error('NEAR account unavailable');
  const held=gasPrice>purchasePrice?gasPrice:purchasePrice;
  return (BigInt(view.storage_usage)+REGISTRATION_BYTES)*perByte+SALE_SEND_GAS*gasPrice+(SALE_PREPAID_GAS+SALE_EXEC_GAS)*held+1n;
}
export async function nearPrices() {
  const [gas,config]=await Promise.all([rpc('gas_price',[null]),rpc('EXPERIMENTAL_protocol_config',{finality:'final'})]);
  return {gasPrice:BigInt(gas.gas_price),perByte:BigInt(config.runtime_config.storage_amount_per_byte),
    purchasePrice:BigInt(config.runtime_config.min_gas_purchase_price??config.min_gas_price??1000000000)};
}
export function account(value) {
  if (typeof value !== 'string' || value.length < 2 || value.length > 64 ||
      !/^[a-z0-9]+(?:[._-][a-z0-9]+)*$/.test(value)) throw new Error('invalid token account');
  return value;
}
export async function rpc(method, params) {
  for(const endpoint of [RPC,'https://rpc.mainnet.fastnear.com']) {
    try {
      const response = await fetch(endpoint,{method:'POST',headers:{'content-type':'application/json'},
        body:JSON.stringify({jsonrpc:'2.0',id:'atlas',method,params}),signal:AbortSignal.timeout(12000)});
      if(!response.ok)continue;
      const body=await response.json();
      if(body.error || body.result?.error)continue;
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
// Search reads the last list at once and refreshes it behind the scenes; swaps keep the 5-minute rule.
let poolRefresh;
export async function pools({stale=false}={}) {
  if(poolCache && Date.now()-poolCache.at<300000)return poolCache.value;
  if(stale && poolCache){poolRefresh??=fetchPools().finally(()=>{poolRefresh=undefined;});poolRefresh.catch(()=>{});return poolCache.value;}
  return fetchPools();
}
async function fetchPools() {
  // The indexer sometimes refuses stale blocks. Fall back to contract views, not stale quotes.
  let value;
  try {
    const r=await fetch('https://indexer.ref.finance/fetchAllPools',{signal:AbortSignal.timeout(10000)});
    const body=await r.json();
    if(Array.isArray(body.simplePools))value=body.simplePools.map(indexedPool);
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
// The indexer lists pools as {id, tokenIds, supplies:{token: amount}, fee, shareSupply}; the contract
// as {token_account_ids, amounts, total_fee, shares_total_supply}. Everything here reads the
// contract's shape: read as-is, every pool looked empty and every NEAR coin "had no liquidity"
// whenever the indexer was up (live, 2026-10-04).
export function indexedPool(p){
  if(Array.isArray(p?.token_account_ids))return p;
  const ids=Array.isArray(p?.tokenIds)?p.tokenIds:[];
  return {id:Number(p?.id),pool_kind:p?.pool_kind??'SIMPLE_POOL',token_account_ids:ids,
    amounts:ids.map(t=>String(p?.supplies?.[t]??'0')),total_fee:p?.fee,shares_total_supply:p?.shareSupply};
}
export function directPools(list,token,nearPrice){
  account(token);
  return list.filter(p=>p.pool_kind==='SIMPLE_POOL' && p.token_account_ids?.length===2 &&
    p.token_account_ids.includes(token) && p.token_account_ids.includes(WRAP) &&
    Number(p.amounts[p.token_account_ids.indexOf(WRAP)])/1e24*nearPrice>=2500);
}
const infoCache=new Map();
export async function tokenInfo(token){
  account(token);
  const held=infoCache.get(token);
  if(held && Date.now()-held.at<3600000)return held.value;
  const metadata=await view(token,'ft_metadata');
  if(!Number.isInteger(metadata.decimals)||metadata.decimals<0||metadata.decimals>24||
      typeof metadata.symbol!=='string'||typeof metadata.name!=='string')throw new Error('token details unavailable');
  infoCache.set(token,{at:Date.now(),value:metadata});
  return metadata;
}

// Search stays fast: Ref's pool, token and price lists load when the bridge starts and refresh
// every four minutes.
export function keepRefWarm(){
  const warm=()=>{fetchPools().catch(()=>{});fetchTokenList().catch(()=>{});fetchPrices().catch(()=>{});};
  warm();
  setInterval(warm,240000).unref();
}
let priceCache,priceRefresh;
// Ref's price list can take 15 s: quotes wait for a fresh one; search uses the last copy meanwhile.
export async function prices({stale=false}={}){
  if(priceCache && Date.now()-priceCache.at<60000)return priceCache.value;
  if(stale && priceCache){priceRefresh??=fetchPrices().finally(()=>{priceRefresh=undefined;});priceRefresh.catch(()=>{});return priceCache.value;}
  return fetchPrices();
}
async function fetchPrices(){
  const r=await fetch('https://indexer.ref.finance/list-token-price',{signal:AbortSignal.timeout(25000)});
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
export async function prepareRef({intentId,wallet,userId,userJwt,accessToken}){
  const scope=await salePermission({intentId,wallet,userId,userJwt,accessToken});
  if(scope.network!=='near'||!['buy','sell'].includes(scope.side))throw new Error('invalid swap permission');
  const sell=scope.side==='sell';account(scope.coinType);
  const balance=BigInt((await rpc('query',{request_type:'view_account',finality:'final',account_id:wallet.address})).amount);
  if(balance<(sell?REF_SALE_GAS:GAS_RESERVE))throw new Error('not enough for the network fee');
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
let listCache,listRefresh;
async function tokenList(){
  if(listCache && Date.now()-listCache.at<300000)return listCache.value;
  if(listCache){listRefresh??=fetchTokenList().finally(()=>{listRefresh=undefined;});listRefresh.catch(()=>{});return listCache.value;}
  return fetchTokenList();
}
async function fetchTokenList(){
  const r=await fetch('https://indexer.ref.finance/list-token',{signal:AbortSignal.timeout(10000)});
  if(!r.ok)throw new Error('token list unavailable');
  listCache={at:Date.now(),value:await r.json()};return listCache.value;
}
export async function searchRef(query){
  if(typeof query!=='string'||query.length<2||query.length>64)return [];
  // Lists load together, and an expired one answers from its last copy while it refreshes.
  // A list that can't load yet doesn't sink the search: prices fall back to the pool's own.
  const [priceMap,catalog,list]=await Promise.all([
    prices({stale:true}).catch(()=>({})),tokenList().catch(()=>({})),pools({stale:true}).catch(()=>[])]);
  const candidates=query.endsWith('.near')?[query]:Object.keys(catalog).filter(k=>
    [catalog[k]?.symbol,catalog[k]?.name].some(v=>typeof v==='string'&&v.toLowerCase().includes(query.toLowerCase()))).slice(0,5);
  const nearPrice=Number(priceMap[WRAP]?.price);
  const found=await Promise.all(candidates.map(async token=>{
    try{
      const metadata=await tokenInfo(token),pairs=directPools(list,token,nearPrice);
      let price=Number(priceMap[token]?.price??0);
      if(!price&&pairs.length){
        const pool=pairs[0],i=pool.token_account_ids.indexOf(token),j=1-i;
        price=Number(pool.amounts[j])/1e24*nearPrice/(Number(pool.amounts[i])/10**metadata.decimals);
      }
      return {token,...metadata,price:String(price),tradeable:pairs.length>0&&Number.isFinite(price)&&price>0};
    }catch{return null;}
  }));
  return found.filter(Boolean);
}

// Where a sale's cash lands: the user's own Solana USDC, or Base USDC without a Solana wallet. The
// engine's `cash_target` picks the same way. (Naira payouts can chain on after this.)
export const SOLANA_USDC='nep141:sol-5ce3bf3a31af18be40ba30f721101b4341690186.omft.near';
export const BASE_USDC='nep141:base-0x833589fcd6edb6e08f4c7c32d4f71b54bda02913.omft.near';
export function cashDestination(solanaWallet,evmWallet){
  if(solanaWallet)return {asset:SOLANA_USDC,recipient:solanaWallet};
  if(/^0x[0-9a-fA-F]{40}$/.test(evmWallet??''))return {asset:BASE_USDC,recipient:evmWallet};
  throw new Error('cash wallet unavailable');
}

// A token's transfer from `from` to `to` in a transaction's receipts, by the token's own event.
export function sentToken(result,token,from,to){
  let amount=0n;
  for(const receipt of result.receipts_outcome??[]){
    if(receipt.outcome?.executor_id!==token||receipt.outcome.status?.Failure)continue;
    for(const log of receipt.outcome.logs??[]){
      if(!log.startsWith('EVENT_JSON:'))continue;
      let event;try{event=JSON.parse(log.slice(11));}catch{continue;}
      if(event.standard!=='nep141'||event.event!=='ft_transfer')continue;
      for(const item of event.data??[])if(item.old_owner_id===from&&item.new_owner_id===to)amount+=BigInt(item.amount);
    }
  }
  return amount;
}

// The deposit to 1Click for a NEAR token: a storage registration for 1Click's address on that token
// when it has none, then the transfer, in one transaction to the token contract (ORIGIN_CHAIN, per
// 1Click's docs: transfer the tokens to the quote's deposit address).
export async function depositActions(token,depositAddress,amount){
  const actions=[];
  if(!(await view(token,'storage_balance_of',{account_id:depositAddress}))){
    const bounds=await view(token,'storage_balance_bounds');
    const minimum=BigInt(bounds.min);
    if(minimum>10000000000000000000000n)throw new Error('token storage cost is too high');
    actions.push(call('storage_deposit',{account_id:depositAddress,registration_only:true},STORAGE_GAS,minimum));
  }
  actions.push(call('ft_transfer',{receiver_id:depositAddress,amount:String(amount)},transferGas(token),1n));
  return actions;
}

const cashouts=new PreparedSales();
// A sale of `amount` of `token` to cash: 1Click's quote (ORIGIN_CHAIN, refunds to the NEAR wallet)
// for the user's own cash wallet, held once for the commit. `scope` comes from a confirmed intent
// (direct sales); otherwise one is made for these exact values (a Ref sale's wNEAR).
export async function prepareCashout({wallet,userId,userJwt,evmWallet,solanaWallet,token=WRAP,amount,minimumOut,scope}){
  account(token);
  const cash=cashDestination(solanaWallet,evmWallet);
  if(BigInt(amount)<=0n||BigInt(minimumOut)<=0n)throw new Error('invalid cashout');
  const nearAccount=await rpc('query',{request_type:'view_account',finality:'final',account_id:wallet.address});
  const native=BigInt(nearAccount.amount),room=saleRoom(nearAccount,await nearPrices());
  if(native<room)throw new Error('not enough NEAR for the network fee');
  if(available(token,BigInt(await view(token,'ft_balance_of',{account_id:wallet.address})),native,room)<BigInt(amount))throw new Error('holding changed');
  // The route must still pay at least the minimum the user was quoted. A fresh quote's own 1% cushion sits
  // under its own price, so it fell below the first quote's minimum whenever the price ticked down in
  // between (the same failure SUI sales had). The price is read first, and the cushion then ends at the
  // quoted minimum (never wider than 1%).
  const route=async(dry,slippageTolerance)=>{
    const request={dry,swapType:'EXACT_INPUT',slippageTolerance,originAsset:`nep141:${token}`,depositType:'ORIGIN_CHAIN',
      destinationAsset:cash.asset,amount:String(amount),recipient:cash.recipient,recipientType:'DESTINATION_CHAIN',
      refundTo:wallet.address,refundType:'ORIGIN_CHAIN',deadline:new Date(Date.now()+240000).toISOString()};
    const response=await fetch('https://1click.chaindefuser.com/v0/quote',{method:'POST',headers:{'content-type':'application/json',...(process.env.NEAR_INTENTS_API_KEY?{'X-API-Key':process.env.NEAR_INTENTS_API_KEY}:{})},body:JSON.stringify(request),signal:AbortSignal.timeout(20000)});
    if(!response.ok)throw new Error('cashout route unavailable');
    return (await response.json()).quote;
  };
  const least=BigInt(minimumOut),moved=()=>new Error('The price moved since your quote');
  let price=BigInt((await route(true,100))?.amountOut??0),q;
  for(let tries=2;;tries--){
    if(price<least)throw moved();
    q=await route(false,Number(min(100n,(price-least)*10000n/price)));
    if(BigInt(q?.minAmountOut??q?.amountOut??0)>=least)break;
    // The price ticked down between the two reads: once more at the newer one.
    if(tries===1)throw moved();
    price=BigInt(q?.amountOut??0);
  }
  checkCashoutQuote(q,amount,minimumOut);
  const bound=scope??{userId,walletId:wallet.id,address:wallet.address,quoteId:randomUUID(),coinType:token,
    amount:String(amount),minimumOut:String(minimumOut),expiresAtUnixMs:Date.now()+180000};
  if(bound.coinType!==token||bound.amount!==String(amount)||bound.minimumOut!==String(minimumOut))throw new Error('sale changed');
  const cashoutId=cashouts.put(bound,userJwt,{depositAddress:q.depositAddress});
  return {cashoutId,scope:bound,depositAddress:q.depositAddress,amountOut:q.amountOut};
}
const min=(a,b)=>(a<b?a:b);
// 1Click's quote must take exactly the amount, pay at least the minimum, and name a plain NEAR
// account to deposit to (no memo).
export function checkCashoutQuote(q,amount,minimumOut){
  if(q?.amountIn!==String(amount)||BigInt(q.minAmountOut??q.amountOut??0)<BigInt(minimumOut)||
    !/^([0-9a-f]{64}|[a-z0-9]+([._-][a-z0-9]+)*\.near)$/.test(q.depositAddress??'')||q.depositMemo!=null)throw new Error('cashout route changed');
}
// A direct sale of a NEAR token the user confirmed: its coin, amount and minimum come only from that
// intent (consumed once by the engine).
export async function prepareSale({intentId,wallet,userId,userJwt,accessToken,evmWallet,solanaWallet}){
  const scope=await salePermission({intentId,wallet,userId,accessToken});
  if(scope.network!=='nearintents'||scope.side!=='sell')throw new Error('invalid sale permission');
  const {walletId,...rest}=scope;
  const bound={userId:rest.userId,walletId,address:rest.address,quoteId:rest.quoteId,coinType:rest.coinType,
    amount:rest.amount,minimumOut:rest.minimumOut,expiresAtUnixMs:rest.expiresAtUnixMs};
  return prepareCashout({wallet,userId,userJwt,evmWallet,solanaWallet,token:bound.coinType,amount:bound.amount,
    minimumOut:bound.minimumOut,scope:bound});
}
// What a wallet can sell of `token`: its balance, and for NEAR also native NEAR beyond what the sale's
// own transaction needs (`room`, saleRoom); 1Click pays NEAR out unwrapped, so that's where most of it sits.
export function available(token,held,native,room){
  return token===WRAP?held+(native>room?native-room:0n):held;
}
// NEAR sold from native NEAR is wrapped first, in the same transaction to wrap.near.
// The wrap, and before it the wallet's own wNEAR registration when it has none: wNEAR registers an
// unregistered wallet out of the NEAR it wraps (0.00125 less arrives), so the transfer after it would come
// up short and the whole sale fail.
export async function wrapActions(token,held,amount,owner){
  const wrap=wrapFirst(token,held,amount);
  if(!wrap.length||await view(WRAP,'storage_balance_of',{account_id:owner}))return wrap;
  const minimum=BigInt((await view(WRAP,'storage_balance_bounds')).min);
  if(minimum>10000000000000000000000n)throw new Error('token storage cost is too high');
  return [call('storage_deposit',{account_id:owner,registration_only:true},STORAGE_GAS,minimum),...wrap];
}
export function wrapFirst(token,held,amount){
  if(token!==WRAP||held>=BigInt(amount))return [];
  return [call('near_deposit',{},WRAP_GAS,BigInt(amount)-held)];
}
export async function commitCashout({cashoutId,scope,wallet,userId,userJwt,rawSign}){
  if(scope.userId!==userId||scope.walletId!==wallet.id||scope.address!==wallet.address)throw new Error('wallet changed');
  const item=cashouts.take(cashoutId,scope,userJwt);
  const token=item.scope.coinType,amount=item.scope.amount,to=item.data.depositAddress;
  const held=BigInt(await view(token,'ft_balance_of',{account_id:wallet.address}));
  const nearAccount=await rpc('query',{request_type:'view_account',finality:'final',account_id:wallet.address});
  const native=BigInt(nearAccount.amount),room=saleRoom(nearAccount,await nearPrices());
  if(available(token,held,native,room)<BigInt(amount)||native<room)throw new Error('holding changed');
  const actions=[...await wrapActions(token,held,amount,wallet.address),...await depositActions(token,to,amount)];
  const result=await sendCall(wallet,token,actions,rawSign,item.scope.expiresAtUnixMs);
  return {ok:sentToken(result,token,wallet.address,to)===BigInt(amount),digest:result.transaction.hash};
}

// Prepare all NEAR transactions with one access-key snapshot and consecutive nonces.
export async function buildNearCalls(wallet,calls) {
  const key=publicKey(wallet),publicKeyString='ed25519:'+toBase58(key);
  const access=await rpc('query',{request_type:'view_access_key',finality:'optimistic',account_id:wallet.address,public_key:publicKeyString});
  if(!Number.isSafeInteger(access.nonce))throw new Error('nonce unavailable');
  return calls.map((c,index)=>{
    const bytes=transaction({signer:wallet.address,publicKey:publicKeyString,nonce:BigInt(access.nonce)+1n+BigInt(index),
      receiver:c.receiver,blockHash:access.block_hash,actions:c.actions});
    return {bytes,receiver:c.receiver,message:'0x'+bytes.toString('hex')};
  });
}
async function storageCall(wallet,token) {
  if(await view(token,'storage_balance_of',{account_id:wallet.address}))return [];
  const minimum=BigInt((await view(token,'storage_balance_bounds')).min);
  if(minimum>10000000000000000000000n)throw new Error('token storage cost is too high');
  return [{receiver:token,actions:[call('storage_deposit',{account_id:wallet.address,registration_only:true},30000000000000n,minimum)]}];
}
export async function buildRef({saleId,scope,wallet,userId,userJwt}) {
  if(scope.userId!==userId||scope.walletId!==wallet.id||scope.address!==wallet.address)throw new Error('wallet changed');
  const item=prepared.take(saleId,scope,userJwt);
  return buildRefTransactions({wallet,...item.data});
}
export async function buildRefTransactions({wallet,q,sell}) {
  const calls=await storageCall(wallet,q.tokenOut);
  if(!sell){
    calls.push(...await storageCall(wallet,WRAP));
    const held=BigInt(await view(WRAP,'ft_balance_of',{account_id:wallet.address}));
    const wrap=BigInt(q.amountIn)>held?BigInt(q.amountIn)-held:0n;
    if(wrap>0n)calls.push({receiver:WRAP,actions:[call('near_deposit',{},30000000000000n,wrap)]});
  }
  calls.push({receiver:q.tokenIn,actions:[swapCall(q)]});
  return {calls:await buildNearCalls(wallet,calls),tokenOut:q.tokenOut,minimumOut:q.minimumOut};
}
export async function buildNearCashout({cashoutId,scope,wallet,userId,userJwt}) {
  if(scope.userId!==userId||scope.walletId!==wallet.id||scope.address!==wallet.address)throw new Error('wallet changed');
  const item=cashouts.take(cashoutId,scope,userJwt);
  const token=item.scope.coinType,amount=item.scope.amount,to=item.data.depositAddress;
  const held=BigInt(await view(token,'ft_balance_of',{account_id:wallet.address}));
  const nearAccount=await rpc('query',{request_type:'view_account',finality:'final',account_id:wallet.address});
  const native=BigInt(nearAccount.amount),room=saleRoom(nearAccount,await nearPrices());
  if(available(token,held,native,room)<BigInt(amount)||native<room)throw new Error('holding changed');
  const actions=[...await wrapActions(token,held,amount,wallet.address),...await depositActions(token,to,amount)];
  return {calls:await buildNearCalls(wallet,[{receiver:token,actions}]),token,amount,to};
}
// What went wrong with a broadcast, in NEAR's own words. NEAR refusing the transaction outright
// (INVALID_TRANSACTION: a balance, nonce or signature problem) means it never ran, so nothing in it was
// sent; only a timeout or an unreadable answer leaves the outcome unknown. A transaction that ran and
// failed moved no coins (its actions are all-or-nothing), though its fee was spent.
export function nearFailure(body,index,sent){
  const step=index+1,earlier=sent.length?` ${sent.length} earlier transaction(s) sent: ${sent.join(', ')}.`:'';
  const reason=v=>JSON.stringify(v??'').slice(0,200);
  if(body?.error){
    const refused=body.error.cause?.name==='INVALID_TRANSACTION';
    // The server adds "nothing was sent" when nothing was (approvalFailure).
    const error=new Error(refused?`NEAR refused step ${step} (${reason(body.error.data??body.error.cause)}).${earlier}`
      :`Step ${step} could not be verified (${reason(body.error.cause??body.error)}).${earlier}`);
    error.sent=[...sent];error.maybeSent=!refused||sent.length>0;
    return error;
  }
  if(body?.result?.status?.Failure){
    const error=new Error(`Step ${step} failed on NEAR (${reason(body.result.status.Failure)}); its coins didn't move.${earlier}`);
    error.sent=[...sent,body.result.transaction?.hash].filter(Boolean);error.maybeSent=sent.length>0;
    return error;
  }
  if(!body?.result?.transaction?.hash){
    const error=new Error(`Step ${step} could not be verified.${earlier}`);
    error.sent=[...sent];error.maybeSent=true;
    return error;
  }
  return null;
}
export async function finishNearCalls({wallet,built,signatures}) {
  if(signatures.length!==built.calls.length)throw new Error('wrong approval count');
  const key=publicKey(wallet),results=[],sent=[];
  // Verify the whole batch before its first submission.
  const signed=built.calls.map((c,index)=>{
    const signature=Buffer.from(signatures[index].replace(/^0x/,''),'hex');
    if(signature.length!==64||!ed25519.verify(signature,sha256(c.bytes),key))throw new Error('signature did not verify');
    return Buffer.concat([c.bytes,Buffer.from([0]),signature]).toString('base64');
  });
  for(let index=0;index<signed.length;index++){
    let body;
    try {
      const response=await fetch(RPC,{method:'POST',headers:{'content-type':'application/json'},
        body:JSON.stringify({jsonrpc:'2.0',id:'atlas',method:'broadcast_tx_commit',params:[signed[index]]}),signal:AbortSignal.timeout(45000)});
      body=await response.json();
    }catch(error){error.sent=sent;error.maybeSent=true;error.message=`Step ${index+1} could not be verified; ${sent.length} earlier transaction(s) sent: ${sent.join(', ')}`;throw error;}
    const failure=nearFailure(body,index,sent);
    if(failure)throw failure;
    results.push(body.result);sent.push(body.result.transaction.hash);
  }
  const last=results.at(-1);
  if(built.tokenOut){
    const amount=receivedToken(last,built.tokenOut,wallet.address);
    return {ok:amount>=BigInt(built.minimumOut),digest:sent.at(-1),txIds:sent,amountOut:amount.toString()};
  }
  return {ok:sentToken(last,built.token,wallet.address,built.to)===BigInt(built.amount),digest:sent.at(-1),txIds:sent};
}
