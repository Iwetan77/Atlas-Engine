import test from 'node:test';
import assert from 'node:assert/strict';
import {privateKeyToAccount} from 'viem/accounts';
import {hashTypedData,keccak256,toHex} from 'viem';
import {commit,walletEnvelopeExpired,walletReceipt,deviceRequests,deviceReport,relayMatches,handle,availabilityView,Approvals,authTyped,depositWallet,orderTyped,wrapOrderSignature,orderQuote,marketView,units,hmac,C,batchTyped,setupCalls,redeemCalls,withdrawCalls,settlementView,positionView} from './polymarket.mjs';
import captured from './polymarket-fixture.mjs';
// Public test vector, never a funded wallet.
const signer=privateKeyToAccount('0x'+'01'.padStart(64,'0'));
const other=privateKeyToAccount('0x'+'02'.padStart(64,'0'));
const scope={owner:signer.address,userId:'test-user',intentId:'prediction-test'};
const quote=()=>orderQuote(captured.market,captured.book,'buy',10000000n);

test('captured live markets pair each outcome with its exact token and price',()=>{
 const m=marketView(captured.market);
 assert.equal(m.outcomes[0].tokenId,captured.book.asset_id);
 assert.equal(m.tradeable,true);
 assert.equal(marketView({...captured.market,outcomePrices:'["NaN","1"]'}),null);
 assert.equal(marketView({...captured.market,closed:true}).tradeable,false);
});
test('a market without its own picture shows its event\'s',()=>{
 const event={icon:'https://polymarket-upload.s3.us-east-2.amazonaws.com/fed.png'};
 assert.equal(marketView({...captured.market,icon:'',image:null,events:[event]}).iconUrl,event.icon);
 assert.equal(marketView({...captured.market,icon:null,image:null}).iconUrl,null);
});
test('captured live book includes fees without exceeding the approved budget',()=>{
 const q=quote();
 assert.ok(BigInt(q.maximumSpend)<=10000000n);
 assert.ok(BigInt(q.feeUnits)>0n);
 assert.equal(BigInt(q.shares)%10000n,0n);
 assert.equal(q.negRisk,captured.market.negRisk);
});
test('limits, liquidity, unknown fees and changed values are refused',()=>{
 assert.throws(()=>orderQuote(captured.market,captured.book,'buy',1000n),/larger/);
 assert.throws(()=>orderQuote(captured.market,{...captured.book,asks:[]},'buy',10000000n),/offers/);
 assert.throws(()=>orderQuote({...captured.market,feeSchedule:null},captured.book,'buy',10000000n),/fee/);
 assert.throws(()=>units('1e4'),/Invalid/);
 assert.throws(()=>units('-1'),/Invalid/);
 assert.throws(()=>units('1.1234567'),/Invalid/);
});
test('deposit wallets are deterministic, owned by the same embedded EVM address',()=>{
 assert.equal(depositWallet('0xEe8646AF9e1DDA672716389aB64a7bD0Fd202ba7'),'0x86025583e6bF787880fD0F065Df9A96f4E42B7a9');
 assert.equal(depositWallet(signer.address),depositWallet(signer.address.toLowerCase()));
 assert.notEqual(depositWallet(signer.address),depositWallet(other.address));
});
test('the signature covers shares, price, market, receiving wallet and exchange',async()=>{
 const typed=orderTyped(signer.address,quote());
 assert.equal(typed.message.contents.maker,depositWallet(signer.address));
 assert.equal(typed.domain.verifyingContract,C.negativeExchange);
 for(const field of ['tokenId','makerAmount','takerAmount','timestamp']){
  const changed=structuredClone(typed);
  changed.message.contents[field]=(BigInt(changed.message.contents[field])+1n).toString();
  assert.notEqual(hashTypedData(typed),hashTypedData(changed));
 }
 const signed=await signer.signTypedData(typed);
 const wrapped=wrapOrderSignature(typed,signed);
 assert.ok(wrapped.startsWith(signed));
 assert.equal(wrapped.slice(-4),('Order(uint256 salt,address maker,address signer,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint8 side,uint8 signatureType,uint256 timestamp,bytes32 metadata,bytes32 builder)').length.toString(16).padStart(4,'0'));
});
test('approval refuses another user, intent, wallet, expiry and altered signed amount',async()=>{
 const store=new Approvals(),typed=orderTyped(signer.address,quote());
 const prepared=store.put(scope.owner,scope.userId,scope.intentId,'order',typed,{});
 const signature=await signer.signTypedData(typed),input={...scope,prepareId:prepared.prepareId,signature};
 await assert.rejects(store.take({...input,userId:'another'}),/another/);
 await assert.rejects(store.take({...input,intentId:'prediction-another'}),/another/);
 await assert.rejects(store.take({...input,owner:other.address}),/another/);
 const changed=structuredClone(typed);changed.message.contents.makerAmount='999999999';
 await assert.rejects(store.take({...input,signature:await signer.signTypedData(changed)}),/wallet/);
 store.items.get(prepared.prepareId).expires=Date.now()-1;
 await assert.rejects(store.take(input),/expired/);
});
test('one-use approvals survive concurrent duplicate reports',async()=>{
 const store=new Approvals(),typed=authTyped(signer.address);
 const p=store.put(scope.owner,scope.userId,scope.intentId,'auth',typed,{});
 const input={...scope,prepareId:p.prepareId,signature:await signer.signTypedData(typed)};
 const results=await Promise.allSettled([store.take(input),store.take(input)]);
 assert.equal(results.filter(r=>r.status==='fulfilled').length,1);
 await assert.rejects(store.take(input),/used/);
});
test('HMAC binds method, path and the exact body including whitespace',()=>{
 const secret=Buffer.from('public-test-vector').toString('base64');
 const a=hmac(secret,'100','POST','/order','{"a":1}');
 for(const args of [['101','POST','/order','{"a":1}'],['100','GET','/order','{"a":1}'],
  ['100','POST','/another','{"a":1}'],['100','POST','/order','{ "a":1 }']]){
  assert.notEqual(a,hmac(secret,...args));
 }
 assert.match(a,/^[A-Za-z0-9_=-]+$/);
});

test('wallet approval, redemption and cash return sign only pinned contracts and exact amounts',async()=>{
 const q={...quote(),side:'buy'},calls=setupCalls(q);
 assert.equal(calls[0].target,C.cash);
 assert.equal(BigInt('0x'+calls[0].data.slice(-64)),BigInt(q.maximumSpend));
 const batch=batchTyped(signer.address,calls,'7',Math.floor(Date.now()/1000)+180);
 const signature=await signer.signTypedData(batch),store=new Approvals();
 const prepared=store.put(scope.owner,scope.userId,scope.intentId,'setup',batch,{});
 const changed=structuredClone(batch);changed.message.calls[0].data=withdrawCalls(other.address,1000000n)[0].data;
 await assert.rejects(store.take({...scope,prepareId:prepared.prepareId,signature:await signer.signTypedData(changed)}),/wallet/);
 assert.equal((await store.take({...scope,prepareId:prepared.prepareId,signature})).kind,'setup');
 const redeem=redeemCalls({negRisk:true,conditionId:captured.market.conditionId});
 assert.equal(redeem[0].target,C.ctf);assert.equal(redeem[1].target,C.negativeAdapter);
 assert.ok(redeem[1].data.toLowerCase().includes(C.cash.slice(2).toLowerCase()));
 const cashout=withdrawCalls(other.address,1234567n);
 assert.equal(cashout[0].target,C.cash);assert.equal(BigInt('0x'+cashout[0].data.slice(-64)),1234567n);
 assert.throws(()=>withdrawCalls('invalid',1n),/address/);
});
test('settlement refuses partial, unrelated and unconfirmed trades',()=>{
 const q=quote(),size=String(Number(q.shares)/1e6),input={orderId:'approved-order',quote:q};
 const order={asset_id:q.tokenId,original_size:size,size_matched:size,status:'MATCHED'};
 const trade={id:'fill-1',taker_order_id:input.orderId,asset_id:q.tokenId,size,status:'CONFIRMED',transaction_hash:'0xsettled'};
 assert.equal(settlementView(order,[trade],input).state,'filled');
 assert.equal(settlementView(order,[{...trade,status:'MINED'}],input).state,'pending');
 assert.equal(settlementView(order,[{...trade,taker_order_id:'other'}],input).state,'pending');
 assert.equal(settlementView(order,[{...trade,size:'1'}],input).state,'pending');
 assert.equal(settlementView(order,[{...trade,status:'FAILED'}],input).state,'failed');
 assert.throws(()=>settlementView({...order,asset_id:'other'},[trade],input),/match/);
 assert.equal(settlementView(order,[trade,trade],input).state,'filled');
});
test('missing position prices fail explicitly instead of showing a zero balance',()=>{
 const p={...captured.position,total_pnl:-2};
 assert.equal(positionView(p).pnlUsd,'-2');
 assert.throws(()=>positionView({...p,current_value:undefined}),/values/);
 assert.throws(()=>positionView({...p,current_size:'NaN'}),/values/);
});

test('availability distinguishes the server connection from user eligibility and omits IPs',()=>{
 const us=availabilityView({blocked:true,country:'US',region:'OR',ip:'private-ip'},true);
 assert.equal(us.serverAllowed,false);
 assert.equal(us.serviceCountry,'US');
 assert.equal(us.blockedBy,'service_region');
 assert.match(us.reason,/server connection/);
 assert.match(us.reason,/separate from your location/);
 assert.equal(JSON.stringify(us).includes('private-ip'),false);
 const ng=availabilityView({blocked:false,country:'NG'},true);
 assert.equal(ng.serverAllowed,true);
 assert.equal(ng.blockedBy,null);
 assert.equal(ng.reason,null);
 const missing=availabilityView({blocked:false,country:'NG'},false);
 assert.equal(missing.blockedBy,'builder_setup');
 assert.match(missing.reason,/credentials/);
 assert.equal(availabilityView({blocked:false,country:'malformed'},true).serviceCountry,null);
 assert.throws(()=>availabilityView({country:'NG'},true),/could not be checked/);
});

const fakeCredentials={apiKey:'public-test-key',secret:Buffer.from('public-secret').toString('base64'),passphrase:'public-passphrase'};
test('device envelope pins order bytes, signer, FOK and HMAC without exposing a secret',async()=>{
 const q={...quote(),side:'buy'},typed=orderTyped(scope.owner,q),signature=await signer.signTypedData(typed);
 const item={...scope,kind:'order',typed,data:{quote:q},expires:Date.now()+45000};
 const envelope=deviceRequests(item,signature,fakeCredentials);
 const order=envelope.requests.find(r=>r.id==='order'),body=JSON.parse(order.body);
 assert.equal(order.url,'https://clob.polymarket.com/order');assert.equal(order.method,'POST');
 assert.equal(body.orderType,'FOK');assert.equal(body.order.maker,depositWallet(scope.owner));
 assert.equal(body.order.makerAmount,q.notional);assert.equal(body.order.tokenId,q.tokenId);
 assert.equal(order.headers.POLY_ADDRESS,scope.owner);
 assert.equal(order.headers.POLY_SIGNATURE,hmac(fakeCredentials.secret,order.headers.POLY_TIMESTAMP,'POST','/order',order.body));
 assert.ok(!JSON.stringify(envelope).includes(fakeCredentials.secret));
 assert.equal(envelope.requests.some(r=>r.method==='POST'&&r.url.includes('onrender.com')),false);
});
test('device wallet envelope signs only the original batch and exact serialized builder body',async()=>{
 const calls=withdrawCalls(other.address,1234567n),typed=batchTyped(scope.owner,calls,'9',Math.floor(Date.now()/1000)+180);
 const signature=await signer.signTypedData(typed),secret=Buffer.from('public-builder-secret').toString('base64');
 const issuer=(method,path,body)=>({POLY_BUILDER_TIMESTAMP:'123',POLY_BUILDER_SIGNATURE:hmac(secret,'123',method,path,JSON.stringify(body))});
 const envelope=deviceRequests({...scope,kind:'withdraw',typed,data:{},expires:Date.now()+180000},signature,null,false,issuer);
 const request=envelope.requests[0],body=JSON.parse(request.body);
 assert.equal(request.url,'https://relayer-v2.polymarket.com/submit');
 assert.deepEqual(body.depositWalletParams.calls,calls);
 assert.equal(body.nonce,'9');assert.equal(body.from,scope.owner);assert.equal(body.to,C.factory);
 assert.equal(body.signature,signature);
 assert.equal(request.headers.POLY_BUILDER_SIGNATURE,hmac(secret,'123','POST','/submit',request.body));
 assert.ok(!JSON.stringify(envelope).includes(secret));
});
test('device auth falls back only on missing key and deployment stays bound to the signed owner',async()=>{
 const typed=authTyped(scope.owner),signature=await signer.signTypedData(typed);
 const envelope=deviceRequests({...scope,kind:'auth',typed,expires:Date.now()+180000},signature,null,true,()=>({}));
 assert.deepEqual(envelope.requests.map(r=>r.id),['auth','authCreate','relay']);
 assert.deepEqual(envelope.requests[1].onFailure,{id:'auth',statuses:[400,404]});
 assert.equal(JSON.parse(envelope.requests[2].body).from,scope.owner);
 assert.equal(envelope.requests[0].headers.POLY_TIMESTAMP,typed.message.timestamp);
});
test('device reports reject plain signatures, duplicates, bad status and missing eligibility',()=>{
 assert.throws(()=>deviceReport('0x123'),/invalid/);
 assert.throws(()=>deviceReport({signature:'sig',results:[],geoAllowed:false}),/device/);
 assert.throws(()=>deviceReport({signature:'sig',geoAllowed:true,results:[{id:'order',status:NaN}]}),/invalid/);
 assert.throws(()=>deviceReport({signature:'sig',geoAllowed:true,results:[{id:'order',status:200},{id:'order',status:200}]}),/invalid/);
 assert.equal(deviceReport({signature:'sig',results:[{id:'order',status:0}],geoAllowed:true}).results[0].status,0);
});
test('relayer settlement rejects another owner, batch, nonce or reused unrelated receipt',()=>{
 const expected={type:'WALLET',from:scope.owner,to:C.factory,nonce:'4',signature:'0x1234'};
 assert.equal(relayMatches(expected,expected),true);
 for(const changed of [{from:other.address},{to:C.cash},{nonce:'5'},{signature:'0x5678'},{type:'SAFE'}])
  assert.equal(relayMatches({...expected,...changed},expected),false);
 const deployment={type:'WALLET-CREATE',from:scope.owner,to:C.factory,metadata:scope.intentId};
 assert.equal(relayMatches(deployment,deployment),true);
 assert.equal(relayMatches({...deployment,metadata:'another'},deployment),false);
});
test('issued reports can be reconciled after expiry but cannot get a fresh device envelope',async()=>{
 const store=new Approvals(),typed=authTyped(scope.owner);
 const p=store.put(scope.owner,scope.userId,scope.intentId,'auth',typed,{});
 const signature=await signer.signTypedData(typed),input={...scope,prepareId:p.prepareId,signature};
 const item=store.items.get(p.prepareId);item.envelope={requests:[]};item.expires=Date.now()-1;
 await assert.rejects(store.check(input),/expired/);
 await store.take({...input,report:true});
 await assert.rejects(store.take({...input,report:true}),/used/);
});
test('prepare and device auth never submit from the server, and commit only verifies the device result',async()=>{
 const savedFetch=globalThis.fetch,vars=['POLYMARKET_BUILDER_API_KEY','POLYMARKET_BUILDER_SECRET','POLYMARKET_BUILDER_PASSPHRASE'];
 const saved=vars.map(k=>process.env[k]);vars.forEach((k,i)=>process.env[k]=['test',fakeCredentials.secret,'test'][i]);
 const calls=[];
 globalThis.fetch=async(url,options={})=>{
  calls.push([String(url),options.method??'GET']);
  let body;
  if(String(url).includes('polygon.drpc.org')){
   const request=JSON.parse(options.body);
   const result=request.method==='eth_chainId'?'0x89':request.method==='eth_getCode'?'0x':
    request.params[0].to.toLowerCase()===C.factory.toLowerCase()?'0x'+C.beacon.slice(2).padStart(64,'0'):'0x6';
   body={jsonrpc:'2.0',id:1,result};
  }else if(String(url).endsWith('/auth/api-keys'))body={apiKeys:[fakeCredentials.apiKey]};
  else throw new Error('Unexpected request: '+url);
  return new Response(JSON.stringify(body),{status:200});
 };
 try{
  const prepared=await handle('prepare',{intentId:scope.intentId,kind:'auth',quote:{deviceSubmission:true}},scope);
  const typed=prepared.transactions[0].typedData,signature=await signer.signTypedData(typed);
  const input={intentId:scope.intentId,prepareId:prepared.prepareId,signature};
  const envelope=await handle('device',input,scope);
  assert.deepEqual(await handle('device',input,scope),envelope);
  const report={signature,geoAllowed:true,results:[{id:'auth',status:200,body:fakeCredentials},{id:'relay',status:200,body:{transactionID:'relay-test'}}]};
  const result=await handle('commit',{intentId:scope.intentId,prepareId:prepared.prepareId,report},scope);
  assert.equal(result.deployId,'relay-test');
  assert.equal(calls.some(([url,method])=>url.includes('polymarket.com')&&method==='POST'),false);
  await assert.rejects(handle('commit',{intentId:scope.intentId,prepareId:prepared.prepareId,report},scope),/used/);
 }finally{globalThis.fetch=savedFetch;vars.forEach((k,i)=>{if(saved[i]===undefined)delete process.env[k];else process.env[k]=saved[i];});}
});

test('a successful receipt must pay the approved recipient and exact amount before cash return advances',()=>{
 const wallet=depositWallet(scope.owner),recipient=other.address,amount=1234567n;
 const expected={depositWalletParams:{calls:withdrawCalls(recipient,amount)}};
 const addressTopic=a=>'0x'+a.slice(2).toLowerCase().padStart(64,'0');
 const log={address:C.cash,topics:[keccak256(toHex('Transfer(address,address,uint256)')),addressTopic(wallet),addressTopic(recipient)],data:'0x'+amount.toString(16).padStart(64,'0')};
 const receipt={status:'0x1',logs:[log]};
 assert.equal(walletReceipt(receipt,expected,wallet),true);
 assert.equal(walletReceipt({...receipt,status:'0x0'},expected,wallet),false);
 assert.equal(walletReceipt({...receipt,logs:[{...log,topics:[...log.topics.slice(0,2),addressTopic(scope.owner)]}]},expected,wallet),false);
 assert.equal(walletReceipt({...receipt,logs:[{...log,data:'0x1'}]},expected,wallet),false);
 assert.equal(walletReceipt({...receipt,logs:[{...log,address:C.ctf}]},expected,wallet),false);
});

test('an unsubmitted wallet batch fails only after chain expiry with its nonce still unused',()=>{
 const expected={type:'WALLET',nonce:'4',depositWalletParams:{deadline:'100'}};
 assert.equal(walletEnvelopeExpired(expected,101n,4n),true);
 assert.equal(walletEnvelopeExpired(expected,100n,4n),false);
 assert.equal(walletEnvelopeExpired(expected,101n,5n),false);
 assert.equal(walletEnvelopeExpired({...expected,type:'WALLET-CREATE'},101n,4n),false);
});

test('HTTP 500 after an order or wallet POST is reconciled instead of allowing another charge',async()=>{
 for(const kind of ['order','setup']){
  const q={...quote(),side:'buy'};
  const typed=kind==='order'?orderTyped(scope.owner,q):batchTyped(scope.owner,setupCalls(q),'7',Math.floor(Date.now()/1000)+180);
  const signature=await signer.signTypedData(typed),store=new Approvals();
  const expectedOrderId=kind==='order'?hashTypedData({domain:typed.domain,types:{Order:typed.types.Order},primaryType:'Order',message:typed.message.contents}):null;
  const prepared=store.put(scope.owner,scope.userId,scope.intentId,kind,typed,{quote:q,expectedOrderId});
  const item=store.items.get(prepared.prepareId);
  item.envelope=deviceRequests(item,signature,fakeCredentials,false,()=>({}));
  item.issuedSignature=signature;
  const report={signature,geoAllowed:true,results:[{id:kind==='order'?'order':'relay',status:500,body:{error:'reply unavailable'}}]};
  const result=await commit(scope.owner,scope.userId,{intentId:scope.intentId,prepareId:prepared.prepareId,report},store);
  assert.equal(result.failure,undefined);
  if(kind==='order'){assert.equal(result.orderId,expectedOrderId);assert.equal(result.submissionUnknown,true);}
  else{assert.equal(result.relayId,null);assert.equal(result.relayRequest.signature,signature);}
  await assert.rejects(commit(scope.owner,scope.userId,{intentId:scope.intentId,prepareId:prepared.prepareId,report},store),/used/);
 }
});
