// Read-only mainnet checks. No wallet signing, authorizing, order placing or broadcasting is allowed.
import {createHash} from 'node:crypto';
import {PrivyClient} from '@privy-io/node';
import {walletBalances,quoteSale,quoteSwap,buildSaleTransaction,buildSuiSwap,buildSuiTransfer,prepareSuiCashout,SUI} from './sui-swap.mjs';
import {rpc,view,quoteRef,buildRefTransactions,buildNearCalls,depositActions,swapCall,WRAP} from './ref-swap.mjs';
import {rawRequest} from './device-approval.mjs';
import {prepareCashOut} from './hyperliquid-cashout.mjs';
import {approveTypedData,agentFor} from './hyperliquid.mjs';
const fetchLive=globalThis.fetch;
globalThis.fetch=(url,init)=>{
  const text=String(url);
  const body=typeof init?.body==='string'&&init.body.trim().startsWith('{')?JSON.parse(init.body):{};
  if(text.includes('/raw_sign')||text.endsWith('/exchange')||text.includes('/authorize')||/executeTransaction/i.test(text)||
     /broadcast|sendTransaction/i.test(body.method??''))throw new Error('dry-run refused a write');
  return fetchLive(url,init);
};
const userId=process.env.ATLAS_DRY_USER_ID;
if(!userId)throw new Error('Set ATLAS_DRY_USER_ID to an existing user id');
const privy=new PrivyClient({appId:process.env.PRIVY_APP_ID,appSecret:process.env.PRIVY_APP_SECRET});
const user=await privy.users()._get('did:privy:'+userId.replace(/^did:privy:/,''));
const evm=user.linked_accounts.find(a=>a.type==='wallet'&&a.chain_type==='ethereum')?.address;
const sol=user.linked_accounts.find(a=>a.type==='wallet'&&a.chain_type==='solana')?.address;
async function walletFor(chain){
  const external_id=`atlas_${chain}_${createHash('sha256').update(user.id).digest('hex').slice(0,32)}`;
  for await(const wallet of privy.wallets().list({external_id}))return wallet;
  throw new Error('existing receiving wallet unavailable');
}
let failures=0;
async function check(name,fn){try{console.log(name+': '+JSON.stringify(await fn()));}catch(e){failures++;console.log(name+': FAIL '+String(e.message).slice(0,220));}}
const sui=await walletFor('sui'),near=await walletFor('near');
const DEEP='0xdeeb7a4662eec9f2f3def03fb937a663dddaa2e215b8078a284d026b7946c270::deep::DEEP';
await check('Sui gRPC balances',async()=>({balances:await walletBalances(sui.address)}));
let sale;
await check('DEEP sale quote + exact unsigned transaction',async()=>{
 const router=await quoteSale(DEEP,'10000000',sui.address);sale=router;
 const approved={address:sui.address,coinType:DEEP,amount:'10000000',minimumOut:(BigInt(router.amountOut)*99n/100n).toString()};
 const built=await buildSaleTransaction({wallet:sui,approved,router});
 return {amountIn:approved.amount,amountOut:router.amountOut.toString(),unsignedBytes:built.txBytes.length,
   request:rawRequest({appId:'redacted-app',walletId:'redacted-wallet',message:built.message,hashFunction:'blake2b256',expires:Date.now()+180000}).request.method};
});
await check('SUI buy quote + exact unsigned transaction',async()=>{
 const router=await quoteSwap(DEEP,'100000000',sui.address);
 const built=await buildSuiSwap({wallet:sui,coinType:DEEP,amount:'100000000',reserve:'20000000',minimumOut:(BigInt(router.amountOut)*99n/100n).toString(),expiresAtUnixMs:Date.now()+180000});
 return {amountIn:built.input.toString(),amountOut:router.amountOut.toString(),unsignedBytes:built.txBytes.length};
});
await check('SUI cashout live quote + unsigned deposit',async()=>{
 const q=await prepareSuiCashout({wallet:sui,evmWallet:evm,solanaWallet:sol,amount:'1000000000',minimumOut:'100000'});
 console.log('1Click SUI cash quote: '+JSON.stringify({amountIn:q.amountIn,amountOut:q.amountOut,minAmountOut:q.minAmountOut}));
 const built=await buildSuiTransfer({wallet:sui,recipient:q.depositAddress,amount:'1000000000',reserve:'20000000'});
 return {amountIn:q.amountIn,amountOut:q.amountOut,unsignedBytes:built.txBytes.length};
});
await check('NEAR access key diagnostics',async()=>{
 const {toBase58}=await import('@mysten/sui/utils');
 const result=await rpc('query',{request_type:'view_access_key',finality:'optimistic',account_id:near.address,
  public_key:'ed25519:'+toBase58(Buffer.from(near.address,'hex'))});
 return {fields:Object.keys(result??{}),nonce:String(result?.nonce),nonceType:typeof result?.nonce};
});
await check('NEAR account + wNEAR balance',async()=>({native:(await rpc('query',{request_type:'view_account',finality:'final',account_id:near.address})).amount,
 wrapped:await view(WRAP,'ft_balance_of',{account_id:near.address})}));
for(const sell of [false,true])await check('Ref '+(sell?'sell':'buy')+' live quote + unsigned batch',async()=>{
 const q=await quoteRef('token.v2.ref-finance.near',sell?'1000000000000000000':'100000000000000000000000',sell);
 console.log('Ref quote: '+JSON.stringify({sell,pool:q.poolId,amountIn:q.amountIn,amountOut:q.amountOut,minimumOut:q.minimumOut}));
 const built=await buildRefTransactions({wallet:near,q,sell});
 return {pool:q.poolId,amountIn:q.amountIn,minimumOut:q.minimumOut,transactions:built.calls.length,bytes:built.calls.map(c=>c.bytes.length)};
});
await check('NEAR direct deposit unsigned transaction',async()=>{
 const actions=await depositActions(WRAP,near.address,'100000000000000000000000');
 const calls=await buildNearCalls(near,[{receiver:WRAP,actions}]);
 return {transactions:calls.length,bytes:calls[0].bytes.length};
});
await check('Hyperliquid agent typed request',async()=>{
 const agent=agentFor(process.env.PRIVY_APP_SECRET,user.id,evm);
 const typed=approveTypedData(agent.address,Date.now());
 return {primaryType:typed.primary_type,chainId:typed.domain.chainId,owner:evm};
});
await check('Hyperliquid cash return live quote + both typed requests',async()=>{
 const q=await prepareCashOut({wallet:evm,recipient:sol??evm,to:sol?'solana':'base',amount:'1000000',relayKey:process.env.RELAY_API_KEY});
 return {amountOut:q.amountOut,mapping:q.mapping.primary_type,send:q.send.primary_type};
});
console.log('DRY RUN: no signatures requested and no transactions broadcast; '+failures+' checks failed');
process.exitCode=failures?1:0;
