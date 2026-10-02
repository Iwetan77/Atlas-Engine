import test from 'node:test';
import assert from 'node:assert/strict';
import {createRequire} from 'node:module';
import {generateKeyPairSync} from 'node:crypto';
import {DeviceApprovals,rawRequest,deviceSign} from './device-approval.mjs';
const wallet={id:'w-1',address:'owned-address'};
const approval='a'.repeat(88);

test('a batch approval cannot change owner, wallet, route, request count or replay',()=>{
  const queue=new DeviceApprovals();
  const prepared=queue.put({userId:'u-1',wallet,appId:'app-123',operation:'/near/ref/commit',
    messages:['0xaabb','0xccdd'],hashFunction:'sha256',data:{amount:'10',minimumOut:'9'}});
  const args={prepareId:prepared.prepareId,userId:'u-1',wallet,operation:'/near/ref/commit',signatures:[approval,approval]};
  for(const change of [{userId:'u-2'},{wallet:{...wallet,id:'other'}},{wallet:{...wallet,address:'other'}},
    {operation:'/near/cashout/commit'},{signatures:[approval]},{signatures:[approval,'bad']}]){
    assert.throws(()=>queue.take({...args,...change}));
  }
  const item=queue.take(args);
  assert.equal(item.data.amount,'10');
  assert.equal(item.requests[1].params.bytes,'0xccdd');
  assert.throws(()=>queue.take(args));
});

test('an expired device approval never reaches Privy',()=>{
  const original=Date.now;
  try {
    Date.now=()=>1000;
    const queue=new DeviceApprovals();
    const p=queue.put({userId:'u',wallet,appId:'a',operation:'commit',messages:['0x00'],hashFunction:'sha256',data:{},expires:2000});
    Date.now=()=>2001;
    assert.throws(()=>queue.take({prepareId:p.prepareId,userId:'u',wallet,operation:'commit',signatures:[approval]}));
  }finally{Date.now=original;}
});

for(const hashFunction of ['sha256','blake2b256'])test(`device ${hashFunction} request matches the Privy SDK byte for byte`,async()=>{
  const require=createRequire(import.meta.url);
  const auth=require(new URL('./node_modules/@privy-io/node/lib/authorization.js',import.meta.url).pathname);
  const key=generateKeyPairSync('ec',{namedCurve:'P-256'}).privateKey.export({format:'der',type:'pkcs8'}).toString('base64');
  const expires=Date.now()+180000;
  const {request,params}=rawRequest({appId:'app-123',walletId:wallet.id,message:'0xabcd',hashFunction,expires});
  const {headers}=await auth.prepareRequest(null,'app-123',{authorizationContext:{authorization_private_keys:[key]},
    requestExpiry:expires,method:'POST',url:request.url,body:{params}});
  const device=auth.generateAuthorizationSignature({authorizationPrivateKey:key,
    input:auth.formatRequestForAuthorizationSignature(structuredClone(request))});
  assert.equal(device,headers['privy-authorization-signature']);
  const calls=[];
  const fake={wallets:()=>({rawSign:async(id,args)=>{calls.push({id,args});return {signature:'chain-signature'};}})};
  assert.deepEqual(await deviceSign(fake,{walletId:wallet.id,requests:[{params,request}],expires,signatures:[device]}),['chain-signature']);
  assert.deepEqual(calls[0].args.authorization_context,{signatures:[device]});
  assert.deepEqual(calls[0].args.params,params);
  assert.equal(calls[0].args.request_expiry,expires);
});
