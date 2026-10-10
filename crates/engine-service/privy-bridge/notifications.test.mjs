
import test from 'node:test';
import assert from 'node:assert/strict';
import {validSubscription,sendWebNotification} from './notifications.mjs';
const subscription={endpoint:'https://web.push.apple.com/test',keys:{p256dh:'A'.repeat(87),auth:'B'.repeat(22)}};
test('web push only accepts pinned push providers',()=>{
 assert.ok(validSubscription(subscription));
 for(const endpoint of ['https://evil.com/x','https://fcm.googleapis.com.evil.com/x','http://127.0.0.1/x','https://user@web.push.apple.com/x']){
  assert.equal(validSubscription({...subscription,endpoint}),false);
 }
});
test('push contains the event summary, stays local and handles expired subscriptions',async()=>{
 let payload;
 const input={subscription,keys:{publicKey:'test',privateKey:'test'},noticeId:'a'.repeat(32),
  title:'You received ₦500.00',body:'₦500.00 arrived from @ade.',url:'/transaction/receipt#private'};
 const result=await sendWebNotification(input,async(_sub,body)=>{payload=JSON.parse(body)});
 assert.equal(result.status,'sent');
 assert.deepEqual(payload,{id:'a'.repeat(32),title:input.title,body:input.body,url:'/transaction/receipt'});
 assert.equal((await sendWebNotification(input,async()=>{throw {statusCode:410}})).status,'expired');
 assert.equal((await sendWebNotification(input,async()=>{throw {statusCode:503}})).status,'retry');
 await sendWebNotification({...input,title:'X'.repeat(500),body:'Y'.repeat(500),url:'//evil.example/'},async(_sub,body)=>{payload=JSON.parse(body)});
 assert.equal(payload.title.length,100);assert.equal(payload.body.length,240);assert.equal(payload.url,'/notifications');
});
