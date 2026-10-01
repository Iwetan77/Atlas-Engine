import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {transaction,swapCall,directPools,receivedToken,account,nextNonce} from './ref-swap.mjs';
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
