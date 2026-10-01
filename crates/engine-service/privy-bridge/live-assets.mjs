// Read-only dry venue checks. Never signs or submits a transaction.
import {writeFile} from 'node:fs/promises';
import {quoteSale} from './sui-swap.mjs';
const DEEP='0xdeeb7a4662eec9f2f3def03fb937a663dddaa2e215b8078a284d026b7946c270::deep::DEEP';
if(process.argv[2]==='sui'){
  const sender='0x'+'1'.repeat(64);
  const metadataResponse=await fetch('https://graphql.mainnet.sui.io/graphql',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({query:'query($coinType:String!){coinMetadata(coinType:$coinType){decimals}}',variables:{coinType:DEEP}}),signal:AbortSignal.timeout(15000)});
  const metadata=(await metadataResponse.json()).data?.coinMetadata;
  if(!Number.isInteger(metadata?.decimals))throw new Error('DEEP metadata unavailable');
  const amount=(100n*10n**BigInt(metadata.decimals)).toString();
  const route=await quoteSale(DEEP,amount,sender);
  if(process.argv.includes('--capture'))await writeFile(new URL('./fixtures/cetus-sale.json',import.meta.url),JSON.stringify({coinType:DEEP,amountIn:amount,amountOut:route.amountOut.toString()},null,2)+String.fromCharCode(10));
  console.log('LIVE DRY CETUS: '+route.amountOut.toString()+' MIST quoted for 100 DEEP');
  const minimum=BigInt(route.amountOut)*99n/100n-20000000n;
  const request={dry:true,swapType:'EXACT_INPUT',slippageTolerance:100,originAsset:'nep141:sui.omft.near',
    depositType:'ORIGIN_CHAIN',destinationAsset:'nep141:sol-5ce3bf3a31af18be40ba30f721101b4341690186.omft.near',
    amount:minimum.toString(),recipient:'So11111111111111111111111111111111111111112',recipientType:'DESTINATION_CHAIN',
    refundTo:sender,refundType:'ORIGIN_CHAIN',deadline:new Date(Date.now()+180000).toISOString()};
  const response=await fetch('https://1click.chaindefuser.com/v0/quote',{
    method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(request),signal:AbortSignal.timeout(30000)});
  const raw=await response.text();
  if(!response.ok)throw new Error('1Click dry quote HTTP '+response.status+' ('+response.headers.get('content-type')+')');
  const body=JSON.parse(raw);
  if(!response.ok)throw new Error(JSON.stringify(body));
  if(BigInt(body.quote.amountOut)<=0n)throw new Error('empty cash quote');
  const fixture={capturedAt:new Date().toISOString(),cetus:{coinType:DEEP,amountIn:amount,
    amountOut:route.amountOut.toString()},request,oneClick:body};
  if(process.argv.includes('--capture'))await writeFile(new URL('./fixtures/sui-sale.json',import.meta.url),JSON.stringify(fixture,null,2)+String.fromCharCode(10));
  console.log('LIVE DRY PASS: 100 DEEP -> '+route.amountOut.toString()+' MIST -> '+body.quote.amountOut+' USDC units on Solana; no funds moved');
}

if(process.argv[2]==='near'){
  const {quoteRef,tokenInfo}=await import('./ref-swap.mjs');
  const token='token.v2.ref-finance.near';
  const metadata=await tokenInfo(token);
  const buy=await quoteRef(token,'1000000000000000000000000');
  const sell=await quoteRef(token,buy.amountOut,true);
  if(process.argv.includes('--capture'))await writeFile(new URL('./fixtures/ref-quote.json',import.meta.url),JSON.stringify({metadata,buy,sell},null,2)+String.fromCharCode(10));
  console.log('LIVE DRY REF: 1 wNEAR -> '+buy.amountOut+' REF units -> '+sell.amountOut+' wNEAR units; no funds moved');
}
