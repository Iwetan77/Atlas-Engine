
import test from 'node:test';
import assert from 'node:assert/strict';
import {historyPoints,priceHistory} from './polymarket.mjs';

test('chart rejects invalid probabilities and future points, sorts and deduplicates timestamps',()=>{
 assert.deepEqual(historyPoints({history:[{t:2,p:0.4},{t:1,p:0.3},{t:2,p:0.5},{t:4,p:1.5},{t:9e9,p:0.3},{t:3,p:NaN}]},10000),
  [[1000,0.3],[2000,0.5]]);
 assert.throws(()=>historyPoints({}),/unavailable/);
 const data=historyPoints({history:Array.from({length:1000},(_,i)=>({t:i+1,p:i/1000}))},1000000);
 assert.equal(data.length,480);assert.deepEqual(data[0],[1000,0]);assert.deepEqual(data.at(-1),[1000000,0.999]);
});
test('both outcome tokens are read independently; duplicate loads share a request; invalid ranges are refused',async()=>{
 const old=globalThis.fetch,calls=[];
 globalThis.fetch=async(raw)=>{
  const url=new URL(raw);calls.push(url);
  if(url.hostname==='gamma-api.polymarket.com')return new Response(JSON.stringify({id:777777,conditionId:'0x'+'a'.repeat(64),outcomes:'["Yes","No"]',outcomePrices:'["0.6","0.4"]',clobTokenIds:'["111","222"]'}));
  const p=url.searchParams.get('market')==='111'?0.6:0.3;
  return new Response(JSON.stringify({history:[{t:1,p},{t:2,p:p+0.01}]}));
 };
 try{
  const [a,b]=await Promise.all([priceHistory({marketId:'777777',range:'1D'}),priceHistory({marketId:'777777',range:'1D'})]);
  assert.deepEqual(a,b);assert.equal(calls.length,3);
  assert.deepEqual(a.series.map(s=>s.points[0][1]),[0.6,0.3]); // Do not invent the second outcome as 1 - first.
  assert.deepEqual(calls.slice(1).map(u=>u.searchParams.get('market')),['111','222']);
  await assert.rejects(priceHistory({marketId:'777777',range:'bad'}),/Invalid chart range/);
  await assert.rejects(priceHistory({marketId:'https://evil.com'}),/Market not found/);
 }finally{globalThis.fetch=old;}
});
