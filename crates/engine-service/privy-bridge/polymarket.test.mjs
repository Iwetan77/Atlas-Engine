import test from 'node:test';
import assert from 'node:assert/strict';
import {privateKeyToAccount} from 'viem/accounts';
import {hashTypedData} from 'viem';
import {availabilityView,Approvals,authTyped,depositWallet,orderTyped,wrapOrderSignature,orderQuote,marketView,units,hmac,C,batchTyped,setupCalls,redeemCalls,withdrawCalls,settlementView,positionView} from './polymarket.mjs';
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
