// Read-only mainnet checks. No signing, deposit address creation or submission.
import {hashTypedData} from 'viem';
import {networkCheck,availability,markets,market,request,account,bridgeQuote,orderQuote,orderTyped,
 batchTyped,setupCalls,redeemCalls,withdrawCalls} from './polymarket.mjs';
const owner=process.env.PREDICTIONS_DRY_OWNER||'0xEe8646AF9e1DDA672716389aB64a7bD0Fd202ba7';
console.log('network',JSON.stringify(await networkCheck()));
console.log('availability',JSON.stringify(await availability()));
const list=await markets();if(!list.markets.length)throw new Error('No open markets returned');
console.log('live markets',list.markets.length);
const {raw,view}=await market(list.markets[0].marketId),token=view.outcomes[0].tokenId;
const book=await request('clob','/book?token_id='+token);
const q={...orderQuote(raw,book,'buy',10000000n),side:'buy'};
console.log('live FOK quote',JSON.stringify({marketId:view.marketId,...q}));
console.log('live sell quote',JSON.stringify(orderQuote(raw,book,'sell',BigInt(q.shares))));
const a=await account(owner);
console.log('live account',JSON.stringify({wallet:a.wallet,cashUnits:a.cashUnits,positions:a.positions.length,deployed:a.deployed}));
for(const from of ['base','solana'])console.log('live funding quote',JSON.stringify(await bridgeQuote(owner,{units:'10000000',from})));
console.log('live return quote',JSON.stringify(await bridgeQuote(owner,{units:'10000000',from:'base',withdraw:true})));
const unsigned=orderTyped(owner,q);
console.log('unsigned order',JSON.stringify({chainId:unsigned.domain.chainId,exchange:unsigned.domain.verifyingContract,
 wallet:unsigned.message.contents.maker,signatureType:unsigned.message.contents.signatureType,digest:hashTypedData(unsigned)}));
const deadline=Math.floor(Date.now()/1000)+180;
// Structural nonce only, never submitted. The authenticated relayer nonce needs Builder credentials.
for(const [kind,calls]of [
 ['setup',setupCalls(q)],['return',withdrawCalls(owner,10000000n)],
 ['claim',redeemCalls({conditionId:view.conditionId,negRisk:view.negRisk})]]){
 const typed=batchTyped(owner,calls,'0',deadline);
 console.log('unsigned wallet batch',JSON.stringify({kind,targets:calls.map(c=>c.target),digest:hashTypedData(typed),nonce:'structural-only'}));
}
console.log('No funds moved. Live user authentication, relayer nonce/deploy and funded order/return/claim remain unverified.');
