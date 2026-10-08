// Pinned Polymarket contracts; every user signature comes from the phone.
import {createHmac,randomUUID,randomBytes} from 'node:crypto';
import {encodeAbiParameters,encodeFunctionData,getCreate2Address,keccak256,toHex,concatHex,recoverTypedDataAddress,parseAbi,hashTypedData} from 'viem';
export const C=Object.freeze({
 cash:'0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB',ctf:'0x4D97DCd97eC945f40cF65F87097ACe5EA0476045',
 exchange:'0xE111180000d2663C0091e4f400237545B87B996B',negativeExchange:'0xe2222d279d744050d28e00520010520000310F59',
 adapter:'0xAdA100Db00Ca00073811820692005400218FcE1f',negativeAdapter:'0xadA2005600Dec949baf300f4C6120000bDB6eAab',
 factory:'0x00000000000Fb5C9ADea0298D729A0CB3823Cc07',beacon:'0x7A18EDfe055488A3128f01F563e5B479D92ffc3a'});
const URLS={gamma:'https://gamma-api.polymarket.com',clob:'https://clob.polymarket.com',data:'https://data-api.polymarket.com',
 relay:'https://relayer-v2.polymarket.com',bridge:'https://bridge.polymarket.com'};
const ZERO='0x'+'00'.repeat(32);
const knownMarkets=new Map();
const erc20=parseAbi(['function balanceOf(address) view returns (uint256)','function approve(address,uint256)','function transfer(address,uint256)']);
const ctf=parseAbi(['function balanceOf(address,uint256) view returns (uint256)','function setApprovalForAll(address,bool)']);
const same=(a,b)=>typeof a==='string'&&typeof b==='string'&&a.toLowerCase()===b.toLowerCase();
const address=a=>{if(!/^0x[0-9a-fA-F]{40}$/.test(a??''))throw new Error('Invalid wallet address');return a;};
export function units(value){const v=String(value);if(!/^\d+(\.\d{1,6})?$/.test(v)||v.length>32)throw new Error('Invalid amount');
 const [a,b='']=v.split('.');return BigInt(a)*1000000n+BigInt(b.padEnd(6,'0'));}
export const decimal=n=>{n=BigInt(n);return (n/1000000n)+'.'+(n%1000000n).toString().padStart(6,'0');};
export const configured=()=>['POLYMARKET_BUILDER_API_KEY','POLYMARKET_BUILDER_SECRET','POLYMARKET_BUILDER_PASSPHRASE'].every(k=>Boolean(process.env[k]?.trim()));
export async function request(service,path,body,headers={}){
 let response;try{response=await fetch(URLS[service]+path,{method:body===undefined?'GET':'POST',
  headers:{'User-Agent':'Mozilla/5.0 Atlas',Accept:'application/json',...(body===undefined?{}:{'Content-Type':'application/json'}),...headers},
  body:body===undefined?undefined:JSON.stringify(body),signal:AbortSignal.timeout(12000)});}
 catch{throw new Error('Polymarket did not answer. Check activity before trying again.');}
 const result=await response.json().catch(()=>null);
 if(!response.ok){const e=new Error('Polymarket returned '+response.status+': '+String(result?.errorMsg??result?.error??result?.message??'unavailable').slice(0,160));e.status=response.status;throw e;}
 return result;
}
export function hmac(secret,timestamp,method,path,body=''){
 return createHmac('sha256',Buffer.from(secret.replace(/-/g,'+').replace(/_/g,'/'),'base64'))
  .update(timestamp+method+path+body).digest('base64').replace(/\+/g,'-').replace(/\//g,'_');
}
function builderHeaders(method,path,body){
 if(!configured())throw new Error('Atlas Builder setup is still needed. Nothing was charged.');
 const timestamp=String(Math.floor(Date.now()/1000));
 return {POLY_BUILDER_API_KEY:process.env.POLYMARKET_BUILDER_API_KEY,POLY_BUILDER_PASSPHRASE:process.env.POLYMARKET_BUILDER_PASSPHRASE,
  POLY_BUILDER_TIMESTAMP:timestamp,POLY_BUILDER_SIGNATURE:hmac(process.env.POLYMARKET_BUILDER_SECRET,timestamp,method,path,body===undefined?'':JSON.stringify(body))};
}
function clobHeaders(owner,c,method,path,body){
 if(!c?.apiKey||!c.secret||!c.passphrase)throw new Error('Approve your Predictions wallet first.');
 const timestamp=String(Math.floor(Date.now()/1000));
 return {POLY_ADDRESS:owner,POLY_API_KEY:c.apiKey,POLY_PASSPHRASE:c.passphrase,POLY_TIMESTAMP:timestamp,
  POLY_SIGNATURE:hmac(c.secret,timestamp,method,path,body===undefined?'':JSON.stringify(body))};
}
const builderCall=(path,body)=>request('relay',path,body,builderHeaders(body===undefined?'GET':'POST',path.split('?')[0],body));
const clobCall=(owner,c,path,body)=>request('clob',path,body,clobHeaders(owner,c,body===undefined?'GET':'POST',path.split('?')[0],body));
async function rpc(method,params){
 const response=await fetch(process.env.POLYGON_RPC_URL?.trim()||'https://polygon.drpc.org',{method:'POST',
  headers:{'Content-Type':'application/json','User-Agent':'Mozilla/5.0'},body:JSON.stringify({jsonrpc:'2.0',id:1,method,params}),signal:AbortSignal.timeout(12000)});
 const body=await response.json();if(!response.ok||body.error||body.result===undefined)throw new Error('Your Predictions balance is temporarily unavailable.');
 return body.result;
}
const read=async(target,abi,functionName,args)=>BigInt(await rpc('eth_call',[{to:target,data:encodeFunctionData({abi,functionName,args})},'latest']));
export function depositWallet(owner){
 address(owner);const id='0x'+owner.slice(2).toLowerCase().padStart(64,'0');
 const args=encodeAbiParameters([{type:'address'},{type:'bytes32'}],[C.factory,id]);
 const code=concatHex([toHex(0x6100523d8160233d3973n+(BigInt((args.length-2)/2)<<56n),{size:10}),C.beacon,
  '0x60195155f3363d3d373d3d363d602036600436635c60da','0x1b60e01b36527fa3f0ad74e5423aebfd80d3ef4346578335a9a72aeaee59ff6c',
  '0xb3582b35133d50545afa5036515af43d6000803e604d573d6000fd5b3d6000f3',args]);
 return getCreate2Address({from:C.factory,salt:keccak256(args),bytecodeHash:keccak256(code)});
}
// Only the provider's country code leaves this check; IP addresses never leave the bridge.
export function availabilityView(geo,isConfigured){
 if(typeof geo?.blocked!=='boolean')throw new Error('Predictions availability could not be checked.');
 const serviceCountry=/^[A-Z]{2}$/.test(geo.country??'')?geo.country:null;
 const blockedBy=geo.blocked?'service_region':!isConfigured?'builder_setup':null;
 const reason=geo.blocked?
  "Polymarket restricts Atlas's server connection. This is separate from your location. You can browse markets while trading is unavailable.":
  !isConfigured?'Atlas Predictions is waiting for its trading credentials. You can browse markets.':null;
 return {configured:isConfigured,serverAllowed:!geo.blocked,serviceCountry,blockedBy,reason,deviceSubmission:true};
}
export async function availability(){
 const r=await fetch('https://polymarket.com/api/geoblock',{signal:AbortSignal.timeout(10000)});
 if(!r.ok)throw new Error('Predictions availability could not be checked.');
 return availabilityView(await r.json(),configured());
}
export async function networkCheck(){
 const [chain,beacon,scale]=await Promise.all([rpc('eth_chainId',[]),
  rpc('eth_call',[{to:C.factory,data:'0x49493a4d'},'latest']),rpc('eth_call',[{to:C.cash,data:'0x313ce567'},'latest'])]);
 if(chain!=='0x89')throw new Error('Predictions network configuration does not match.');
 if(!same('0x'+beacon.slice(-40),C.beacon))throw new Error('Predictions wallet factory changed; nothing was charged.');
 if(BigInt(scale)!==6n)throw new Error('Predictions cash scale changed; nothing was charged.');
 return {chainId:137,factoryVerified:true,cashDecimals:6};
}
async function ready(deviceSubmission){
 if(deviceSubmission!==true)throw new Error('Update Atlas to submit Predictions from your own device. Nothing was charged.');
 if(!configured())throw new Error('Atlas Builder setup is still needed. Nothing was charged.');
 await networkCheck();
}

const array=v=>Array.isArray(v)?v:typeof v==='string'?JSON.parse(v):[];
export function marketView(m){
 const labels=array(m.outcomes),prices=array(m.outcomePrices),ids=array(m.clobTokenIds);
 if(!/^\d+$/.test(String(m.id))||!/^0x[0-9a-fA-F]{64}$/.test(m.conditionId??'')||labels.length!==2||prices.length!==2||ids.length!==2||ids.some(v=>!/^\d+$/.test(v))||
  prices.some(v=>!Number.isFinite(Number(v))||Number(v)<0||Number(v)>1))return null;
 return {marketId:String(m.id),conditionId:m.conditionId,question:m.question,description:m.description??'',iconUrl:m.icon||m.image||m.events?.[0]?.icon||m.events?.[0]?.image||null,
  endDate:m.endDate??null,volumeUsd:String(m.volumeNum??m.volume??0),closed:m.closed===true,
  tradeable:m.active===true&&m.closed===false&&m.acceptingOrders===true,
  outcomes:labels.map((label,i)=>({label,tokenId:ids[i],probability:String(prices[i])})),negRisk:m.negRisk===true};
}
export async function markets({q='',offset=0}={}){
 if(q.length>100||!Number.isInteger(offset)||offset<0||offset>2000)throw new Error('Invalid search');
 const raw=q.trim()?(await request('gamma','/public-search?q='+encodeURIComponent(q.trim())+'&limit_per_type=20&search_profiles=false')).events?.flatMap(e=>e.markets??[])??[]:
  await request('gamma','/markets?closed=false&active=true&limit=30&order=volume24hr&ascending=false&offset='+offset);
 for(const m of raw)if(m.conditionId)knownMarkets.set(m.conditionId,String(m.id));
 return {markets:raw.map(m=>{try{return marketView(m);}catch{return null;}}).filter(m=>m?.tradeable),
  nextOffset:q?null:raw.length===30?offset+30:null};
}
export async function market(id){
 if(!/^\d{1,15}$/.test(id??''))throw new Error('Market not found');const raw=await request('gamma','/markets/'+id),view=marketView(raw);
 if(!view)throw new Error('Market details unavailable');knownMarkets.set(view.conditionId,view.marketId);return {raw,view};
}
const histories=new Map();
export function historyPoints(body, now=Date.now()) {
 if(!Array.isArray(body?.history))throw new Error('Prediction history is unavailable.');
 const points=new Map();
 for(const row of body.history){
  if(typeof row?.t!=='number'||!Number.isFinite(row.t)||row.t<=0||row.t*1000>now+60000||
     typeof row?.p!=='number'||!Number.isFinite(row.p)||row.p<0||row.p>1)continue;
  points.set(Math.round(row.t*1000),row.p);
 }
 const sorted=[...points].sort((a,b)=>a[0]-b[0]);
 if(sorted.length<=480)return sorted;
 return Array.from({length:480},(_,i)=>sorted[Math.round(i*(sorted.length-1)/479)]);
}
export async function priceHistory({marketId,range='1D'}){
 const ranges={'1D':['1d',5],'1W':['1w',30],'1M':['1m',120],ALL:['max',360]};
 if(!Object.hasOwn(ranges,range))throw new Error('Invalid chart range');
 if(!/^\d{1,15}$/.test(marketId??''))throw new Error('Market not found');
 const key=marketId+':'+range, cached=histories.get(key);
 if(cached&&cached.expires>Date.now())return cached.promise;
 const promise=(async()=>{
  const {view}=await market(marketId),[interval,fidelity]=ranges[range];
  const series=await Promise.all(view.outcomes.map(async outcome=>{
   const body=await request('clob','/prices-history?'+new URLSearchParams({market:outcome.tokenId,interval,fidelity:String(fidelity)}));
   return {label:outcome.label,tokenId:outcome.tokenId,points:historyPoints(body)};
  }));
  return {marketId,range,series};
 })();
 if(histories.size>=100)histories.clear();
 const slot={expires:Date.now()+30000,promise};histories.set(key,slot);
 try{return await promise;}catch(error){if(histories.get(key)===slot)histories.delete(key);throw error;}
}
export function positionView(p){
 const token=p.token_id;
 if(!/^\d+$/.test(token??'')||!/^0x[0-9a-fA-F]{64}$/.test(p.condition_id??'')||
   ![p.current_size,p.current_value,p.total_pnl].every(v=>v!==null&&v!==undefined&&Number.isFinite(Number(v)))||
   Number(p.current_size)<0||Number(p.current_value)<0)throw new Error('Prediction position values unavailable.');
 return {positionId:token,tokenId:token,marketId:String(knownMarkets.get(p.condition_id)??''),
  conditionId:p.condition_id,question:p.title??'',outcome:p.outcome,shares:String(p.current_size),valueUsd:String(p.current_value),
  pnlUsd:String(p.total_pnl),redeemable:p.redeemable===true,negRisk:p.negative_risk===true,iconUrl:p.icon??null};
}
async function positions(wallet,status){
 const rows=[];let cursor=null;
 for(let page=0;page<20;page++){
  const result=await request('data','/v2/positions?user='+wallet+'&size_threshold=0&limit=100&status='+status+(cursor?'&cursor='+encodeURIComponent(cursor):''));
  if(!Array.isArray(result?.data))throw new Error('Prediction positions unavailable');
  rows.push(...result.data);
  if(result.pagination?.has_more!==true)return rows;
  const next=result.pagination?.next_cursor;
  if(typeof next!=='string'||!next||next===cursor)throw new Error('The full Predictions balance could not be read.');
  cursor=next;
 }
 throw new Error('The full Predictions balance could not be read.');
}
export async function account(owner){
 const wallet=depositWallet(owner);
 const [cash,code,...statuses]=await Promise.all([read(C.cash,erc20,'balanceOf',[wallet]),rpc('eth_getCode',[wallet,'latest']),
  ...['OPEN','REDEEMABLE','REDEEMABLE_LOST'].map(status=>positions(wallet,status))]);
 const rows=[...new Map(statuses.flat().map(p=>[p.token_id,p])).values()];
 const missing=[...new Set(rows.filter(p=>!knownMarkets.has(p.condition_id)).map(p=>p.condition_id))];
 for(let offset=0;offset<missing.length;offset+=20){
  const conditions=missing.slice(offset,offset+20);
  if(conditions.some(c=>!/^0x[0-9a-fA-F]{64}$/.test(c??'')))throw new Error('Prediction position details unavailable.');
  const found=await request('gamma','/markets?limit=20&condition_ids='+conditions.join(','));
  for(const condition of conditions){
   const matched=found.find(m=>m.conditionId===condition);
   if(!matched)throw new Error('Prediction position market unavailable.');
   knownMarkets.set(condition,String(matched.id));
  }
 }
 return {wallet,cashUnits:cash.toString(),deployed:code!=='0x',positions:rows.map(positionView)};
}

function feePerShare(raw,p){
 if(raw.feesEnabled===false)return 0n;const f=raw.feeSchedule;
 if(!f||!Number.isFinite(Number(f.rate))||f.exponent!==1||f.takerOnly!==true)throw new Error('This market fee is unavailable. Nothing was charged.');
 const rate=units(String(f.rate));if(rate>1000000n)throw new Error('Invalid market fee');
 return (rate*p*(1000000n-p)+999999999999n)/1000000000000n;
}
// FOK means all the quoted shares or none. Sizes are rounded down, cash never rounded up past its budget.
export function orderQuote(raw,book,side,budget){
 if(!['buy','sell'].includes(side)||budget<=0n)throw new Error('Invalid prediction amount');
 const levels=((side==='buy'?book.asks:book.bids)??[]).map(l=>({price:units(l.price),size:units(l.size)}))
  .filter(l=>l.price>0n&&l.price<1000000n&&l.size>0n).sort((a,b)=>a.price===b.price?0:(a.price<b.price?-1:1)*(side==='buy'?1:-1));
 if(!levels.length)throw new Error('No offers for this outcome right now.');
 const tick=units(book.tick_size),minimum=units(String(book.min_order_size));
 if(tick<=0n||minimum<=0n)throw new Error('Market limits unavailable');
 const best=levels[0].price;
 let limit=side==='buy'?((best*101n/100n+tick-1n)/tick)*tick:((best*99n/100n)/tick)*tick;
 limit=limit<tick?tick:limit>1000000n-tick?1000000n-tick:limit;
 const fee=feePerShare(raw,limit);let shares=(side==='buy'?(budget>10n?budget-10n:0n)*1000000n/(limit+fee):budget)/10000n*10000n;
 const available=levels.filter(l=>side==='buy'?l.price<=limit:l.price>=limit).reduce((n,l)=>n+l.size,0n);
 if(shares>available)throw new Error('Not enough offers at this price. Try a smaller amount.');
 const notional=shares*limit/1000000n;
 if(shares<minimum||notional<minimum){
  const e=new Error('This market needs a larger order.');
  // A buy's smallest budget, not the bare minimum: enough shares for the market's share and dollar
  // minimums at this limit, in 0.01-share steps, with the fee and the 10-unit margin on top.
  if(side==='buy'){
   let need=(minimum*1000000n+limit-1n)/limit;if(need<minimum)need=minimum;need=(need+9999n)/10000n*10000n;
   e.minimumUnits=((need*(limit+fee)+999999n)/1000000n+10n).toString();
  }else e.minimumUnits=minimum.toString();
  throw e;
 }
 const feeUnits=(shares*fee/1000000n+9n)/10n*10n;
 return {shares:shares.toString(),limit:limit.toString(),notional:notional.toString(),feeUnits:feeUnits.toString(),
  maximumSpend:(notional+feeUnits).toString(),minimumReceive:(notional>feeUnits?notional-feeUnits:0n).toString(),
  tokenId:book.asset_id,negRisk:book.neg_risk===true,tickSize:book.tick_size};
}
export async function preview(owner,input){
 await ready(input.deviceSubmission);const {raw,view}=await market(input.marketId);
 if(!view.tradeable)throw new Error('This market has stopped taking orders.');
 const outcome=view.outcomes.find(o=>o.tokenId===input.tokenId);if(!outcome)throw new Error('Outcome does not belong to this market');
 const book=await request('clob','/book?token_id='+input.tokenId);
 if(book.market!==view.conditionId||book.asset_id!==input.tokenId||book.neg_risk!==view.negRisk)throw new Error('Market book did not match.');
 if(!Number.isFinite(Number(book.timestamp))||Date.now()-Number(book.timestamp)>60000)throw new Error('Price is stale. Request a fresh quote.');
 const q=orderQuote(raw,book,input.side,BigInt(input.units)),wallet=depositWallet(owner);
 if(input.side==='sell'&&await read(C.ctf,ctf,'balanceOf',[wallet,BigInt(input.tokenId)])<BigInt(q.shares))throw new Error('You do not hold that many shares.');
 return {...q,marketId:view.marketId,question:view.question,outcome:outcome.label,conditionId:view.conditionId,side:input.side,wallet,
  expiresAtUnixMs:Date.now()+60000,iconUrl:view.iconUrl};
}
const BASE_USDC='0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913',SOL_USDC='EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v';
export async function bridgeQuote(owner,{units:amount,from='base',withdraw=false,solanaWallet}){
 if(!['base','solana'].includes(from))throw new Error('Unsupported cash source');
 const wallet=depositWallet(owner);amount=BigInt(amount);
 const chain=from==='solana'?'1151111081099710':'8453',token=from==='solana'?SOL_USDC:BASE_USDC;
 const recipient=withdraw?(from==='solana'?solanaWallet:owner):wallet;
 if(!recipient)throw new Error('Cash wallet unavailable');
 const assets=await request('bridge','/supported-assets'),row=assets.supportedAssets?.find(a=>String(a.chainId)===chain&&same(a.token.address,token));
 if(!row||row.token.decimals!==6)throw new Error('Cash transfers unavailable.');
 if(Number(decimal(amount))<Number(row.minCheckoutUsd)){const e=new Error('Transfer below the minimum.');e.minimumUnits=units(String(row.minCheckoutUsd)).toString();throw e;}
 const q=await request('bridge','/quote',{fromAmountBaseUnit:amount.toString(),fromChainId:withdraw?'137':chain,fromTokenAddress:withdraw?C.cash:token,
  recipientAddress:recipient,toChainId:withdraw?chain:'137',toTokenAddress:withdraw?token:C.cash});
 if(!/^\d+$/.test(q.estToTokenBaseUnit??'')||BigInt(q.estToTokenBaseUnit)<=0n)throw new Error('Cash transfer quote unavailable');
 return {units:amount.toString(),receiveUnits:q.estToTokenBaseUnit,feeUnits:(amount>BigInt(q.estToTokenBaseUnit)?amount-BigInt(q.estToTokenBaseUnit):0n).toString(),
  from,withdraw,wallet,recipient};
}
export async function deposit(owner,input){
 await ready(input.deviceSubmission);const r=await request('bridge','/deposit',{address:depositWallet(owner)});address(r?.address?.evm);
 if(typeof r.address.svm!=='string'||r.address.svm.length<32)throw new Error('Deposit address unavailable');return r.address;
}

export function authTyped(owner){
 return {domain:{name:'ClobAuthDomain',version:'1',chainId:137},types:{ClobAuth:[{name:'address',type:'address'},
  {name:'timestamp',type:'string'},{name:'nonce',type:'uint256'},{name:'message',type:'string'}]},primaryType:'ClobAuth',
  message:{address:address(owner),timestamp:String(Math.floor(Date.now()/1000)),nonce:'0',message:'This message attests that I control the given wallet'}};
}
const ORDER_FIELDS=[['salt','uint256'],['maker','address'],['signer','address'],['tokenId','uint256'],['makerAmount','uint256'],
 ['takerAmount','uint256'],['side','uint8'],['signatureType','uint8'],['timestamp','uint256'],['metadata','bytes32'],['builder','bytes32']]
 .map(([name,type])=>({name,type}));
export function orderTyped(owner,q){
 const wallet=depositWallet(owner),buy=q.side==='buy';
 const contents={salt:BigInt('0x'+randomBytes(6).toString('hex')).toString(),maker:wallet,signer:wallet,tokenId:q.tokenId,
  makerAmount:buy?q.notional:q.shares,takerAmount:buy?q.shares:q.notional,side:buy?0:1,signatureType:3,timestamp:String(Date.now()),metadata:ZERO,builder:ZERO};
 return {domain:{name:'Polymarket CTF Exchange',version:'2',chainId:137,verifyingContract:q.negRisk?C.negativeExchange:C.exchange},
  types:{Order:ORDER_FIELDS,TypedDataSign:[{name:'contents',type:'Order'},{name:'name',type:'string'},{name:'version',type:'string'},
   {name:'chainId',type:'uint256'},{name:'verifyingContract',type:'address'},{name:'salt',type:'bytes32'}]},primaryType:'TypedDataSign',
  message:{contents,name:'DepositWallet',version:'1',chainId:137,verifyingContract:wallet,salt:ZERO}};
}
export function wrapOrderSignature(typed,signature){
 const d=typed.domain,type='Order('+ORDER_FIELDS.map(f=>f.type+' '+f.name).join(',')+')',contents=typed.message.contents;
 const separator=keccak256(encodeAbiParameters([{type:'bytes32'},{type:'bytes32'},{type:'bytes32'},{type:'uint256'},{type:'address'}],
  [keccak256(toHex('EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)')),
   keccak256(toHex(d.name)),keccak256(toHex(d.version)),137n,d.verifyingContract]));
 const hash=keccak256(encodeAbiParameters([{type:'bytes32'},...ORDER_FIELDS.map(f=>({type:f.type}))],
  [keccak256(toHex(type)),...ORDER_FIELDS.map(f=>contents[f.name])]));
 return concatHex([signature,separator,hash,toHex(type),toHex(type.length,{size:2})]);
}
export class Approvals{
 items=new Map();
 put(owner,userId,intentId,kind,typed,data,expires=Date.now()+180000){
  for(const [id,v]of this.items)if(v.expires+(v.envelope?600000:0)<=Date.now())this.items.delete(id);
  if(this.items.size>=1000)throw new Error('Predictions queue is busy.');
  if(!intentId.startsWith('prediction-')||expires<=Date.now()||expires>Date.now()+180000)throw new Error('Invalid prediction approval');
  const prepareId=randomUUID();
  this.items.set(prepareId,{owner,userId,intentId,kind,typed:structuredClone(typed),data:structuredClone(data),expires});
  return {prepareId,transactions:[{chain:'polygon',typedData:typed,prediction:{prepareId,intentId,expiresAtUnixMs:expires}}],expiresAtUnixMs:expires};
 }
 async check({prepareId,owner,userId,intentId,signature,report}){
  const item=this.items.get(prepareId);
  if(!item||!same(item.owner,owner)||item.userId!==userId||item.intentId!==intentId||item.expires+(report&&item.envelope?600000:0)<=Date.now())
   throw new Error('Approval expired, used, or belongs to another purchase.');
  if(!/^0x[0-9a-fA-F]{130}$/.test(signature??''))throw new Error('Invalid prediction signature.');
  const recovered=await recoverTypedDataAddress({...item.typed,signature});
  if(!same(owner,recovered))throw new Error('Approval does not belong to your wallet.');
  // Delete before a network call; a lost reply must never submit it again.
  return item;
 }
 async take(input){const item=await this.check(input);if(this.items.get(input.prepareId)!==item)throw new Error('Approval already used.');this.items.delete(input.prepareId);return item;
 }
}
const approvals=new Approvals();
export function batchTyped(owner,calls,nonce,deadline){
 const wallet=depositWallet(owner);
 if(!/^\d+$/.test(String(nonce))||!Number.isInteger(deadline)||deadline<=Math.floor(Date.now()/1000))throw new Error('Wallet nonce unavailable');
 return {domain:{name:'DepositWallet',version:'1',chainId:137,verifyingContract:wallet},
  types:{Call:[{name:'target',type:'address'},{name:'value',type:'uint256'},{name:'data',type:'bytes'}],
   Batch:[{name:'wallet',type:'address'},{name:'nonce',type:'uint256'},{name:'deadline',type:'uint256'},{name:'calls',type:'Call[]'}]},
  primaryType:'Batch',message:{wallet,nonce:String(nonce),deadline:String(deadline),calls}};
}
export async function batch(owner,calls){
 const r=await request('relay','/nonce?address='+owner+'&type=WALLET');
 return batchTyped(owner,calls,r.nonce,Math.floor(Date.now()/1000)+180);
}
export function setupCalls(q){
 const exchange=q.negRisk?C.negativeExchange:C.exchange;
 return q.side==='buy'?[{target:C.cash,value:'0',data:encodeFunctionData({abi:erc20,functionName:'approve',args:[exchange,BigInt(q.maximumSpend)]})}]:
  [{target:C.ctf,value:'0',data:encodeFunctionData({abi:ctf,functionName:'setApprovalForAll',args:[exchange,true]})}];
}
export function redeemCalls(q){
 const target=q.negRisk?C.negativeAdapter:C.adapter;
 if(!/^0x[0-9a-fA-F]{64}$/.test(q.conditionId??''))throw new Error('Resolution details unavailable.');
 return [{target:C.ctf,value:'0',data:encodeFunctionData({abi:ctf,functionName:'setApprovalForAll',args:[target,true]})},
  {target,value:'0',data:encodeFunctionData({abi:parseAbi(['function redeemPositions(address,bytes32,bytes32,uint256[])']),
    functionName:'redeemPositions',args:[C.cash,ZERO,q.conditionId,[1n,2n]]})}];
}
export function withdrawCalls(recipient,amount){
 address(recipient);amount=BigInt(amount);if(amount<=0n)throw new Error('Invalid cash amount');
 return [{target:C.cash,value:'0',data:encodeFunctionData({abi:erc20,functionName:'transfer',args:[recipient,amount]})}];
}

export async function redemption(owner,tokenId){
 const wallet=depositWallet(owner),a=await account(owner);
 const p=a.positions.find(p=>p.tokenId===tokenId&&p.redeemable);
 if(!p||!/^0x[0-9a-fA-F]{64}$/.test(p.conditionId??''))throw new Error('This position is not ready to claim.');
 const [raw]=await request('gamma','/markets?condition_ids='+p.conditionId);
 if(!raw||raw.conditionId!==p.conditionId||raw.closed!==true)throw new Error('Resolution details unavailable.');
 const tokens=array(raw.clobTokenIds);if(!tokens.includes(tokenId))throw new Error('Winning position does not match this event.');
 const resolutionAbi=parseAbi(['function payoutDenominator(bytes32) view returns (uint256)',
  'function payoutNumerators(bytes32,uint256) view returns (uint256)']);
 const denominator=await read(C.ctf,resolutionAbi,'payoutDenominator',[p.conditionId]);
 if(denominator===0n)throw new Error('This event is not resolved yet.');
 let payout=0n;
 for(let i=0;i<tokens.length;i++){
  const holding=await read(C.ctf,ctf,'balanceOf',[wallet,BigInt(tokens[i])]);
  const numerator=await read(C.ctf,resolutionAbi,'payoutNumerators',[p.conditionId,BigInt(i)]);
  payout+=holding*numerator/denominator;
 }
 if(payout<=0n)throw new Error('There are no winning shares to claim.');
 return {side:'redeem',tokenId,conditionId:p.conditionId,question:p.question,outcome:p.outcome,iconUrl:p.iconUrl,negRisk:raw.negRisk===true,
  maximumSpend:'0',minimumReceive:payout.toString(),feeUnits:'0',units:'0',expiresAtUnixMs:Date.now()+60000};
}
export async function prepare(owner,userId,input){
 await ready(input.quote?.deviceSubmission);const q=input.quote,kind=input.kind,wallet=depositWallet(owner);let typed,data={};
 if(kind==='auth')typed=authTyped(owner);
 else if(kind==='setup'){
  typed=await batch(owner,setupCalls(q));
 }else if(kind==='order'){
  const {raw,view}=await market(q.marketId);
  if(!view.tradeable||!view.outcomes.some(o=>o.tokenId===q.tokenId)||view.conditionId!==q.conditionId||view.negRisk!==q.negRisk)
   throw new Error('This market changed. Your unused cash stays in Predictions.');
  const book=await request('clob','/book?token_id='+q.tokenId),limit=BigInt(q.limit),shares=BigInt(q.shares);
  if(book.asset_id!==q.tokenId||book.market!==q.conditionId||book.neg_risk!==q.negRisk||
    Date.now()-Number(book.timestamp)>60000||!Number.isFinite(Number(book.timestamp)))throw new Error('The current market book is unavailable.');
  if(limit%units(book.tick_size)!==0n||shares<units(book.min_order_size)||
    BigInt(q.notional)<units(book.min_order_size))throw new Error('The market minimum changed. Request a fresh quote.');
  const depth=(q.side==='buy'?book.asks:book.bids).filter(l=>q.side==='buy'?units(l.price)<=limit:units(l.price)>=limit)
   .reduce((n,l)=>n+units(l.size),0n);
  const fee=(shares*feePerShare(raw,limit)/1000000n+9n)/10n*10n;
  if(depth<shares||fee>BigInt(q.feeUnits))throw new Error('The confirmed price is no longer available. Your unused cash stays in Predictions.');
  q.expiresAtUnixMs=Date.now()+45000;
  const cash=await read(C.cash,erc20,'balanceOf',[wallet]);
  if(q.side==='buy'&&cash<BigInt(q.maximumSpend))throw new Error('Your Predictions cash has not arrived yet.');
  if(q.side==='sell'&&await read(C.ctf,ctf,'balanceOf',[wallet,BigInt(q.tokenId)])<BigInt(q.shares))throw new Error('Your share balance changed.');
  typed=orderTyped(owner,q);data={quote:q,expectedOrderId:hashTypedData({domain:typed.domain,types:{Order:ORDER_FIELDS},primaryType:'Order',message:typed.message.contents})};
 }else if(kind==='redeem'){
  const checked=await redemption(owner,q.tokenId);
  if(checked.conditionId!==q.conditionId||checked.negRisk!==q.negRisk||
    BigInt(checked.minimumReceive)<BigInt(q.minimumReceive))throw new Error('Your winning balance changed.');
  typed=await batch(owner,redeemCalls(q));
 }else if(kind==='withdraw'){
  const cash=await read(C.cash,erc20,'balanceOf',[wallet]),amount=BigInt(input.units);
  if(amount<=0n||amount>cash)throw new Error('Not enough Predictions cash.');
  const b=await bridgeQuote(owner,{units:input.units,from:input.from,withdraw:true,solanaWallet:input.solanaWallet});
  if(BigInt(b.receiveUnits)<BigInt(input.minimumOut))throw new Error('Withdrawal price changed. Request a fresh quote.');
  const r=await request('bridge','/withdraw',{address:wallet,toChainId:input.from==='solana'?'1151111081099710':'8453',
   toTokenAddress:input.from==='solana'?SOL_USDC:BASE_USDC,recipientAddr:b.recipient}),recipient=address(r?.address?.evm);
  typed=await batch(owner,withdrawCalls(recipient,amount));
  data={depositAddress:recipient,receiveUnits:b.receiveUnits};
 }else throw new Error('Unsupported prediction action');
 return {...approvals.put(owner,userId,input.intentId,kind,typed,data,
  Math.min(Date.now()+180000,kind==='order'?q.expiresAtUnixMs:Date.now()+180000)),...data};
}
// The server signs only short-lived HMAC headers for requests rebuilt from a checked wallet approval.
export function deviceRequests(item,signature,credentials,undeployed=false,authBuilder=builderHeaders){
 const typed=item.typed,owner=item.owner,requests=[];
 const add=(id,service,path,body,headers,onFailure)=>{
  const method=body===undefined?'GET':'POST';
  requests.push({id,url:URLS[service]+path,method,headers:{Accept:'application/json',...(body===undefined?{}:{'Content-Type':'application/json'}),...headers},
   ...(body===undefined?{}:{body:JSON.stringify(body)}),...(onFailure?{onFailure}: {})});
 };
 if(item.kind==='auth'){
  const headers={POLY_ADDRESS:owner,POLY_SIGNATURE:signature,POLY_TIMESTAMP:typed.message.timestamp,POLY_NONCE:typed.message.nonce};
  add('auth','clob','/auth/derive-api-key',undefined,headers);
  add('authCreate','clob','/auth/api-key',{},headers,{id:'auth',statuses:[400,404]});
  if(undeployed){
   const body={type:'WALLET-CREATE',from:owner,to:C.factory,metadata:item.intentId};
   add('relay','relay','/submit',body,authBuilder('POST','/submit',body));
  }
 }else if(item.kind==='order'){
  const q=item.data.quote,c=typed.message.contents;
  const order={...c,salt:Number(c.salt),side:c.side===0?'BUY':'SELL',expiration:'0',signature:wrapOrderSignature(typed,signature)};
  const paths=['/balance-allowance/update?asset_type=COLLATERAL&signature_type=3'];
  if(q.side==='sell')paths.push('/balance-allowance/update?asset_type=CONDITIONAL&token_id='+q.tokenId+'&signature_type=3');
  paths.forEach((path,i)=>add('allowance'+i,'clob',path,undefined,clobHeaders(owner,credentials,'GET',path.split('?')[0])));
  const body={deferExec:false,order,orderType:'FOK',owner:credentials.apiKey};
  add('order','clob','/order',body,clobHeaders(owner,credentials,'POST','/order',body));
 }else if(['setup','withdraw','redeem'].includes(item.kind)){
  const body={type:'WALLET',from:owner,to:C.factory,nonce:typed.message.nonce,signature,metadata:item.intentId,
   depositWalletParams:{depositWallet:typed.message.wallet,deadline:typed.message.deadline,calls:typed.message.calls}};
  add('relay','relay','/submit',body,authBuilder('POST','/submit',body));
 }else throw new Error('Unsupported prediction action');
 return {expiresAtUnixMs:item.expires,requests};
}
export async function device(owner,userId,input){
 const item=await approvals.check({...input,owner,userId});
 if(item.envelope){
  if(item.issuedSignature!==input.signature)throw new Error('This approval was already prepared with a different signature.');
  return item.envelope;
 }
 // Check deployment before building the envelope, then recheck ownership after this asynchronous read.
 const undeployed=item.kind==='auth'&&await rpc('eth_getCode',[depositWallet(owner),'latest'])==='0x';
 await approvals.check({...input,owner,userId});
 if(!item.envelope){item.envelope=deviceRequests(item,input.signature,input.credentials,undeployed);item.issuedSignature=input.signature;}
 return item.envelope;
}
export function deviceReport(raw){
 let report;try{report=typeof raw==='string'?JSON.parse(raw):raw;}catch{throw new Error('Prediction device report is invalid.');}
 if(!report||typeof report.signature!=='string'||!Array.isArray(report.results)||report.results.length>5||report.geoAllowed!==true)
  throw new Error('Prediction must be submitted from your own device.');
 const seen=new Set();
 for(const r of report.results){
  if(!r||typeof r.id!=='string'||seen.has(r.id)||!Number.isInteger(r.status)||r.status<0||r.status>599)
   throw new Error('Prediction device report is invalid.');
  seen.add(r.id);
 }
 return report;
}
const signingInput=input=>({...input,signature:deviceReport(input.report).signature});
const successful=r=>r&&r.status>=200&&r.status<300;
const relayId=r=>{const id=r?.body?.transactionID;return typeof id==='string'&&/^[a-zA-Z0-9-]{1,100}$/.test(id)?id:null;};
function failure(result){
 if(!result||result.status===0)return 'The device lost its connection. Check Activity before trying again.';
 const message=String(result.body?.errorMsg??result.body?.error??result.body?.message??'The request was refused.').slice(0,180);
 return result.status===403?'Predictions is not available from this device connection. Nothing new was purchased.':'Predictions returned '+result.status+': '+message;
}
export async function commit(owner,userId,input,store=approvals){
 const report=deviceReport(input.report),item=await store.check({...signingInput(input),owner,userId,report:true});
 if(!item.envelope||report.signature!==item.issuedSignature||report.results.some(r=>!item.envelope.requests.some(q=>q.id===r.id)))
  throw new Error('Device report does not match the prepared action.');
 await store.take({...signingInput(input),owner,userId,report:true});
 const result=id=>report.results.find(r=>r.id===id),relay=result('relay');
 const relayRequest=item.envelope.requests.find(r=>r.id==='relay');
 const expected=relayRequest?JSON.parse(relayRequest.body):null;
 if(item.kind==='auth'){
  const auth=successful(result('auth'))?result('auth'):result('authCreate'),credentials=auth?.body;
  if(!successful(auth)||!credentials?.apiKey||!credentials.secret||!credentials.passphrase)throw new Error(failure(auth));
  // Authenticate the returned credentials independently before allowing any cash transfer.
  await clobCall(owner,credentials,'/auth/api-keys');
  if(expected&&relay?.status&&relay.status<500&&!successful(relay))throw new Error(failure(relay));
  if(expected&&!relay)throw new Error('Wallet setup was not submitted. Nothing was charged.');
  return {credentials,deployId:relayId(relay),deployment:Boolean(expected),relayRequest:expected};
 }
 if(item.kind==='order'){
  const order=result('order'),orderId=item.data.expectedOrderId;
  if(order?.status&&order.status<500&&(!successful(order)||order.body?.success!==true))return {failure:failure(order)};
  if(successful(order)&&!same(order.body?.orderID,orderId))throw new Error('Order receipt does not match your approved order.');
  const prerequisite=report.results.find(r=>r.id.startsWith('allowance')&&!successful(r));
  if(!order)return {failure:prerequisite?failure(prerequisite):'The approval expired before the order was sent. Request a fresh quote.'};
  return {orderId,submissionUnknown:!successful(order),txIds:[]};
 }
 if(relay?.status&&relay.status<500&&!successful(relay))return {failure:failure(relay)};
 if(!relay)throw new Error('The wallet action was not submitted. Nothing was charged.');
 return {relayId:relayId(relay),relayRequest:expected,...item.data};
}
export function relayMatches(row,expected){
 if(!row||!expected||!same(row.from,expected.from)||!same(row.to,expected.to)||row.type!==expected.type)return false;
 if(expected.type==='WALLET')return String(row.nonce)===expected.nonce&&same(row.signature,expected.signature);
 return row.metadata===expected.metadata;
}
export function settlementView(order,rows,input){
 const result={state:'pending',txIds:[]};
 if(['CANCELED','UNMATCHED'].includes(String(order.status).toUpperCase()))return {...result,state:'failed'};
 if(!Array.isArray(rows))throw new Error('Trade settlement unavailable');
 const related=rows.filter(t=>t.taker_order_id===input.orderId);
 if(related.some(t=>t.status==='FAILED'))return {...result,state:'failed'};
 if(!related.length||related.some(t=>t.status!=='CONFIRMED'))return result;
 const q=input.quote;
 if(!q||order.asset_id!==q.tokenId||units(order.original_size)!==BigInt(q.shares))throw new Error('Order settlement does not match the approved shares.');
 const seen=new Set();let size=0n;
 for(const t of related){
  if(t.asset_id!==q.tokenId||!t.id||!t.transaction_hash)throw new Error('Trade confirmation details unavailable.');
  if(!seen.has(t.id)){size+=units(t.size);seen.add(t.id);}
 }
 if(size!==BigInt(q.shares)||units(order.size_matched)!==BigInt(q.shares))return result;
 return {state:'filled',txIds:[...new Set(related.map(t=>t.transaction_hash))]};
}
// A relayer acknowledgement alone is never a completed wallet action.
export function walletReceipt(receipt,expected,wallet,minimumReceive='0'){
 if(receipt?.status!=='0x1'||!Array.isArray(receipt.logs))return false;
 const topicAddress=a=>'0x'+a.slice(2).toLowerCase().padStart(64,'0');
 const topic=event=>keccak256(toHex(event));
 const matches=(log,target,event,from,to)=>same(log.address,target)&&same(log.topics?.[0],topic(event))&&
  same(log.topics?.[1],topicAddress(from))&&same(log.topics?.[2],topicAddress(to));
 let redeem=false;
 for(const call of expected.depositWalletParams?.calls??[]){
  if(same(call.target,C.cash)&&call.data.startsWith('0x095ea7b3')){
   const spender='0x'+call.data.slice(34,74),amount=BigInt('0x'+call.data.slice(74,138));
   if(!receipt.logs.some(l=>matches(l,C.cash,'Approval(address,address,uint256)',wallet,spender)&&BigInt(l.data)===amount))return false;
  }else if(same(call.target,C.ctf)&&call.data.startsWith('0xa22cb465')){
   const operator='0x'+call.data.slice(34,74);
   if(!receipt.logs.some(l=>matches(l,C.ctf,'ApprovalForAll(address,address,bool)',wallet,operator)&&BigInt(l.data)===1n))return false;
  }else if(same(call.target,C.cash)&&call.data.startsWith('0xa9059cbb')){
   const recipient='0x'+call.data.slice(34,74),amount=BigInt('0x'+call.data.slice(74,138));
   if(!receipt.logs.some(l=>matches(l,C.cash,'Transfer(address,address,uint256)',wallet,recipient)&&BigInt(l.data)===amount))return false;
  }else if(same(call.target,C.adapter)||same(call.target,C.negativeAdapter))redeem=true;
  else return false;
 }
 if(redeem){
  let net=0n;
  for(const log of receipt.logs.filter(l=>same(l.address,C.cash)&&same(l.topics?.[0],topic('Transfer(address,address,uint256)')))){
   if(same(log.topics?.[2],topicAddress(wallet)))net+=BigInt(log.data);
   if(same(log.topics?.[1],topicAddress(wallet)))net-=BigInt(log.data);
  }
  if(net<=0n||net<BigInt(minimumReceive))return false;
 }
 return Boolean(expected.depositWalletParams?.calls?.length);
}
export function walletEnvelopeExpired(expected,blockTimestamp,nonce){
 return expected?.type==='WALLET'&&/^\d+$/.test(expected.depositWalletParams?.deadline??'')&&
  BigInt(blockTimestamp)>BigInt(expected.depositWalletParams.deadline)&&BigInt(nonce)===BigInt(expected.nonce);
}
async function missingWalletAction(expected,wallet){
 const deadline=Number(expected.depositWalletParams?.deadline);
 if(expected.type!=='WALLET'||!Number.isFinite(deadline)||Date.now()<deadline*1000+30000)return {state:'pending',txIds:[]};
 const [block,nonce]=await Promise.all([rpc('eth_getBlockByNumber',['latest',false]),
  read(wallet,parseAbi(['function nonce() view returns (uint256)']),'nonce',[])]);
 return {state:walletEnvelopeExpired(expected,block.timestamp,nonce)?'failed':'pending',txIds:[]};
}
export async function progress(owner,input){
 if(input.deployment||input.relayRequest){
  const expected=input.relayRequest,wallet=depositWallet(owner);
  if(!expected||!same(expected.from,owner)||!same(expected.to,C.factory))throw new Error('Wallet settlement does not match your action.');
  if(expected.type==='WALLET-CREATE'&&await rpc('eth_getCode',[wallet,'latest'])!=='0x')
   return {state:'filled',txIds:[],wallet};
  let rows;
  if(input.relayId){
   try{rows=await request('relay','/transaction?id='+encodeURIComponent(input.relayId));}
   catch(e){if(e.status===404)return missingWalletAction(expected,wallet);throw e;}
  }else rows=await builderCall('/transactions');
  const row=(Array.isArray(rows)?rows:[]).find(r=>relayMatches(r,expected));
  if(!row)return missingWalletAction(expected,wallet);
  if(['STATE_FAILED','STATE_INVALID'].includes(row.state))return {state:'failed',txIds:[]};
  if(row.state!=='STATE_CONFIRMED'||!/^0x[0-9a-fA-F]{64}$/.test(row.transactionHash??''))return {state:'pending',txIds:[]};
  const receipt=await rpc('eth_getTransactionReceipt',[row.transactionHash]);
  if(!receipt)return {state:'pending',txIds:[]};
  if(receipt.status!=='0x1')return {state:'failed',txIds:[row.transactionHash]};
  if(expected.type==='WALLET-CREATE'&&await rpc('eth_getCode',[wallet,'latest'])==='0x')return {state:'pending',txIds:[]};
  if(expected.type==='WALLET'&&!walletReceipt(receipt,expected,wallet,input.quote?.minimumReceive))
   throw new Error('Wallet settlement did not match the approved action. Check Activity.');
  return {state:'filled',txIds:[row.transactionHash],wallet};
 }
 if(input.orderId){
  let o;
  try{o=await clobCall(owner,input.credentials,'/data/order/'+encodeURIComponent(input.orderId));}
  catch(e){if(e.status===404)return {state:'pending',txIds:[]};throw e;}
  const r=await clobCall(owner,input.credentials,'/data/trades?market='+encodeURIComponent(o.market)),rows=Array.isArray(r)?r:r.data;
  return settlementView(o,rows,input);
 }
 if(input.depositAddress)return {bridge:await request('bridge','/status/'+encodeURIComponent(input.depositAddress))};
 return account(owner);
}
export async function handle(route,input,{owner,userId,solanaWallet}){
 if(!owner)throw new Error('Your Atlas wallet is not ready.');
 switch(route){
 case 'chart':return priceHistory(input);
  case 'markets':return markets(input);
 case 'market':return (await market(input.marketId)).view;
 case 'availability':return availability();
 case 'account':return account(owner);
 case 'preview':return preview(owner,input);
 case 'redemption':return redemption(owner,input.tokenId);
 case 'bridge-quote':return bridgeQuote(owner,{...input,solanaWallet});
 case 'deposit':return deposit(owner,input);
 case 'prepare':return prepare(owner,userId,{...input,solanaWallet});
 case 'device':return device(owner,userId,input);
 case 'check':await approvals.check({...signingInput(input),owner,userId,report:true});return {valid:true};
 case 'commit':return commit(owner,userId,input);
 case 'progress':return progress(owner,input);
 default:throw new Error('Unsupported prediction route');
 }
}
