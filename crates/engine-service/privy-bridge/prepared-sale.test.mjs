import assert from 'node:assert/strict';
import {test} from 'node:test';
import {PreparedSales} from './prepared-sale.mjs';
const scope={userId:'u',walletId:'w',address:'a',quoteId:'q',coinType:'coin',amount:'100',
  minimumOut:'99',expiresAtUnixMs:2000};
test('a sale binds coin, amount, minimum, user, wallet, quote, expiry and identity token',()=>{
  for(const [field,value] of [['coinType','other'],['amount','101'],['minimumOut','98'],
    ['userId','other'],['walletId','other'],['address','other'],['quoteId','other'],['expiresAtUnixMs',3000]]){
    const store=new PreparedSales();const id=store.put(scope,'identity',{},1000);
    assert.throws(()=>store.take(id,{...scope,[field]:value},'identity',1001));
    assert.deepEqual(store.take(id,scope,'identity',1001).scope,scope);
    assert.throws(()=>store.take(id,scope,'identity',1001));
  }
  const store=new PreparedSales();const id=store.put(scope,'identity',{},1000);
  assert.throws(()=>store.take(id,scope,'different',1001));
  assert.throws(()=>store.take(id,scope,'identity',2000));
});
