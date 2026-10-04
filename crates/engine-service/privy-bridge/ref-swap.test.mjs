import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {transaction,swapCall,directPools,indexedPool,receivedToken,account,nextNonce,checkCashoutQuote,sentToken,cashDestination,available,wrapFirst,depositActions,GAS_RESERVE} from './ref-swap.mjs';
import {PreparedSales} from './prepared-sale.mjs';
const fixture=name=>JSON.parse(readFileSync(new URL('./fixtures/'+name,import.meta.url)));
test('NEAR encoding matches the official near-api-js 7.3.1 vector',()=>{
  const v=fixture('near-transaction.json');
  assert.equal(transaction(v.input).toString('hex'),v.hex);
  assert.throws(()=>transaction({...v.input,nonce:'-1'}));
  assert.throws(()=>transaction({...v.input,receiver:'../another'}));
});
test('captured Ref quotes bind the exact pool, amount and minimum in both directions',()=>{
  const f=fixture('ref-quote.json');
  for(const q of [f.buy,f.sell]){
    const action=swapCall(q),args=action.args,msg=JSON.parse(args.msg);
    assert.equal(args.receiver_id,'v2.ref-finance.near');assert.equal(args.amount,q.amountIn);
    assert.deepEqual(msg.actions,[{pool_id:q.poolId,token_in:q.tokenIn,token_out:q.tokenOut,amount_in:q.amountIn,min_amount_out:q.minimumOut}]);
    assert.equal(action.deposit,'1');
    assert.throws(()=>swapCall({...q,minimumOut:'0'}));
  }
});
test('thin or unrelated Ref pools cannot make an asset tradeable',()=>{
  const p={id:79,pool_kind:'SIMPLE_POOL',token_account_ids:['wrap.near','token.v2.ref-finance.near'],amounts:['1000000000000000000000000000','1']};
  assert.equal(directPools([p],'token.v2.ref-finance.near',3).length,1);
  assert.equal(directPools([p],'token.v2.ref-finance.near',1).length,0);
  assert.equal(directPools([p],'another.near',3).length,0);
  // The same pool as Ref's indexer lists it (shape captured live, 2026-10-04) is found the same way.
  const indexed=indexedPool({id:7,tokenIds:p.token_account_ids,fee:30,shareSupply:'1',pool_kind:'SIMPLE_POOL',
    supplies:Object.fromEntries(p.token_account_ids.map((t,i)=>[t,p.amounts[i]]))});
  assert.equal(directPools([indexed],'token.v2.ref-finance.near',3).length,1);
  assert.equal(indexed.id,7);
  assert.equal(indexedPool(p),p);
  assert.throws(()=>account('bad/token'));
});
test('Ref settlement counts only venue transfers of the output into this wallet',()=>{
  const event=(executor,from,to,amount)=>({outcome:{executor_id:executor,logs:['EVENT_JSON:'+JSON.stringify({standard:'nep141',event:'ft_transfer',data:[{old_owner_id:from,new_owner_id:to,amount}]})],status:{SuccessValue:''}}});
  const receipts=[event('wrap.near','v2.ref-finance.near','alice.near','123'),event('other.near','v2.ref-finance.near','alice.near','999'),event('wrap.near','another.near','alice.near','999')];
  assert.equal(receivedToken({receipts_outcome:receipts},'wrap.near','alice.near'),123n);
});
test('a Ref preparation rejects changes to coin, holding, minimum, owner, token, expiry and replay',()=>{
  const now=1000,scope={userId:'u',walletId:'w',address:'alice.near',quoteId:'q',coinType:'token.v2.ref-finance.near',amount:'10',minimumOut:'9',expiresAtUnixMs:2000};
  const p=new PreparedSales(),id=p.put(scope,'identity-token',{},now);
  for(const change of [{coinType:'other.near'},{amount:'11'},{minimumOut:'8'},{userId:'other'},{expiresAtUnixMs:3000}])assert.throws(()=>p.take(id,{...scope,...change},'identity-token',now));
  assert.throws(()=>p.take(id,scope,'other-token',now));
  assert.throws(()=>p.take(id,scope,'identity-token',2001));
  p.take(id,scope,'identity-token',now);assert.throws(()=>p.take(id,scope,'identity-token',now));
});
test('transactions sent in a row take consecutive nonces even when the node lags',()=>{
  const chain={};
  assert.equal(nextNonce(100,chain),101n);
  // The node still reports 100 after the first went out: the next one must not reuse 101.
  assert.equal(nextNonce(100,chain),102n);
  // The node caught up past what this run used: follow the node.
  assert.equal(nextNonce(150,chain),151n);
  assert.equal(nextNonce(7,{}),8n);
});

// Captured from 1Click on 2026-10-01: 2,000 SHITZU to Solana USDC (ORIGIN_CHAIN, not funded).
const SHITZU_QUOTE={amountIn:'2000000000000000000000',amountOut:'7792945',minAmountOut:'7715015',
  depositAddress:'7478b3f96ffcaffb848a85a269195600a6201f30658fbf671029a5480da89c7d',depositMemo:null};
test("1Click's deposit quote must take exactly the amount, pay the minimum, to a plain NEAR account",()=>{
  checkCashoutQuote(SHITZU_QUOTE,'2000000000000000000000','7715015');
  assert.throws(()=>checkCashoutQuote(SHITZU_QUOTE,'2000000000000000000001','7715015'),/changed/);
  assert.throws(()=>checkCashoutQuote(SHITZU_QUOTE,'2000000000000000000000','7715016'),/changed/);
  assert.throws(()=>checkCashoutQuote({...SHITZU_QUOTE,depositMemo:'123'},'2000000000000000000000','1'),/changed/);
  assert.throws(()=>checkCashoutQuote({...SHITZU_QUOTE,depositAddress:'evil/../x'},'2000000000000000000000','1'),/changed/);
  checkCashoutQuote({...SHITZU_QUOTE,depositAddress:'deposit-1.intents.near'},'2000000000000000000000','1');
});
test('a sale counts only the token moving from this wallet to the deposit address',()=>{
  const log=(executor,from,to,amount)=>({outcome:{executor_id:executor,status:{SuccessValue:''},logs:['EVENT_JSON:'+
    JSON.stringify({standard:'nep141',event:'ft_transfer',data:[{old_owner_id:from,new_owner_id:to,amount}]})]}});
  const result={receipts_outcome:[log('token.0xshitzu.near','me','dep','5'),log('token.0xshitzu.near','me','other','9'),
    log('other.near','me','dep','9'),{outcome:{executor_id:'token.0xshitzu.near',status:{Failure:{}},logs:[]}}]};
  assert.equal(sentToken(result,'token.0xshitzu.near','me','dep'),5n);
});
test('cash lands in the Solana wallet, else Base, else nowhere',()=>{
  assert.equal(cashDestination('SoLWallet1111111111111111111111111111111111','0x'+'1'.repeat(40)).recipient,'SoLWallet1111111111111111111111111111111111');
  assert.equal(cashDestination(null,'0x'+'1'.repeat(40)).asset.startsWith('nep141:base-'),true);
  assert.throws(()=>cashDestination(null,'nope'));
});
test("NEAR sells from native NEAR beyond the gas reserve, wrapping only what's missing",()=>{
  const one=10n**24n;
  assert.equal(available('wrap.near',0n,one),one-GAS_RESERVE);
  assert.equal(available('wrap.near',2n,GAS_RESERVE/2n),2n);
  assert.equal(available('token.0xshitzu.near',7n,one),7n);
  assert.deepEqual(wrapFirst('wrap.near',3n,'10').map(a=>[a.method,a.deposit]),[['near_deposit','7']]);
  assert.deepEqual(wrapFirst('wrap.near',10n,'10'),[]);
  assert.deepEqual(wrapFirst('token.0xshitzu.near',0n,'10'),[]);
});
test("the deposit registers 1Click's address on the token only when it isn't yet, then transfers",async()=>{
  const realFetch=globalThis.fetch;
  const answer=(value)=>new Response(JSON.stringify({result:{result:[...Buffer.from(JSON.stringify(value))]}}));
  try{
    let registered=null;
    globalThis.fetch=async(url,init)=>{
      const body=JSON.parse(init.body),method=body.params.method_name;
      if(method==='storage_balance_of')return answer(registered);
      if(method==='storage_balance_bounds')return answer({min:'1250000000000000000000',max:null});
      throw new Error('unexpected call '+method);
    };
    const fresh=await depositActions('token.0xshitzu.near','7478b3f96ffcaffb848a85a269195600a6201f30658fbf671029a5480da89c7d','2000');
    assert.deepEqual(fresh.map(a=>a.method),['storage_deposit','ft_transfer']);
    assert.equal(fresh[0].deposit,'1250000000000000000000');
    assert.deepEqual(fresh[1].args,{receiver_id:'7478b3f96ffcaffb848a85a269195600a6201f30658fbf671029a5480da89c7d',amount:'2000'});
    assert.equal(fresh[1].deposit,'1');
    registered={total:'1250000000000000000000',available:'0'};
    const again=await depositActions('token.0xshitzu.near','7478b3f96ffcaffb848a85a269195600a6201f30658fbf671029a5480da89c7d','2000');
    assert.deepEqual(again.map(a=>a.method),['ft_transfer']);
  }finally{globalThis.fetch=realFetch;}
});
test('a deposit with its storage registration encodes as one transaction of two actions',()=>{
  const v=fixture('near-transaction.json');
  const actions=[{method:'storage_deposit',args:{account_id:'a.near',registration_only:true},gas:'30000000000000',deposit:'1250000000000000000000'},
    {method:'ft_transfer',args:{receiver_id:'a.near',amount:'1'},gas:'30000000000000',deposit:'1'}];
  const bytes=transaction({...v.input,actions});
  // signer (u32 length + bytes), key type and key, nonce, receiver (u32 length + bytes), block hash.
  const at=4+v.input.signer.length+1+32+8+4+v.input.receiver.length+32;
  assert.equal(bytes.readUInt32LE(at),2);
  // Each action is a FunctionCall (2) naming its method.
  assert.equal(bytes[at+4],2);
  assert.equal(bytes.subarray(at+9,at+9+15).toString(),'storage_deposit');
});


test('device NEAR batch builds consecutive nonces from one chain read and verifies every signature before sending',async()=>{
  const {buildNearCalls,finishNearCalls}=await import('./ref-swap.mjs');
  const {ed25519}=await import('@noble/curves/ed25519');
  const {sha256}=await import('@noble/hashes/sha256');
  const {toBase58}=await import('@mysten/sui/utils');
  const key=new Uint8Array(32).fill(7),pub=ed25519.getPublicKey(key);
  const wallet={id:'w',address:Buffer.from(pub).toString('hex'),public_key:'ed25519:'+toBase58(pub)};
  const original=globalThis.fetch;let reads=0,broadcasts=0;
  try{
    globalThis.fetch=async(_url,init)=>{
      const body=JSON.parse(init.body);
      if(body.method==='query'){reads++;return new Response(JSON.stringify({result:{nonce:100,block_hash:'11111111111111111111111111111111'}}));}
      broadcasts++;
      if(broadcasts===2)throw new Error('connection dropped');
      return new Response(JSON.stringify({result:{status:{SuccessValue:''},transaction:{hash:'landed-1'},receipts_outcome:[]}}));
    };
    const action={method:'near_deposit',args:{},gas:'30000000000000',deposit:'1'};
    const calls=await buildNearCalls(wallet,[{receiver:'wrap.near',actions:[action]},{receiver:'wrap.near',actions:[action]}]);
    assert.equal(reads,1);
    const offset=4+wallet.address.length+1+32;
    assert.equal(calls[0].bytes.readBigUInt64LE(offset),101n);
    assert.equal(calls[1].bytes.readBigUInt64LE(offset),102n);
    const signatures=calls.map(c=>'0x'+Buffer.from(ed25519.sign(sha256(c.bytes),key)).toString('hex'));
    await assert.rejects(()=>finishNearCalls({wallet,built:{calls},signatures:[signatures[0],'0x'+'00'.repeat(64)]}),/signature did not verify/);
    assert.equal(broadcasts,0);
    await assert.rejects(()=>finishNearCalls({wallet,built:{calls},signatures}),error=>{
      assert.deepEqual(error.sent,['landed-1']);assert.equal(error.maybeSent,true);
      assert.match(error.message,/Step 2/);return true;
    });
  }finally{globalThis.fetch=original;}
});
