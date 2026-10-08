
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
test('push payload never contains money details and expired subscriptions are removed',async()=>{
 let payload;
 const input={subscription,keys:{publicKey:'test',privateKey:'test'},noticeId:'a'.repeat(32),amount:'₦10000',recipient:'alice'};
 const result=await sendWebNotification(input,async(_sub,body)=>{payload=JSON.parse(body)});
 assert.equal(result.status,'sent');
 assert.deepEqual(payload,{id:'a'.repeat(32),title:'Atlas',body:'You have a new Atlas money update.',url:'/notifications'});
 assert.equal((await sendWebNotification(input,async()=>{throw {statusCode:410}})).status,'expired');
 assert.equal((await sendWebNotification(input,async()=>{throw {statusCode:503}})).status,'retry');
});
