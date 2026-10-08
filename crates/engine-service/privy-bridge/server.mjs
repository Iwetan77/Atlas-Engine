import {notificationKeys,sendWebNotification} from './notifications.mjs';
import {handle as predictions,availability as predictionAvailability} from './polymarket.mjs';
function approvalFailure(error,fallback){const reason=error.message??fallback;return error.maybeSent||error.sent?.length?reason:reason+'; nothing was sent';}
import {DeviceApprovals,deviceSign} from './device-approval.mjs';
import {coinBalance,walletBalances} from './sui-swap.mjs';
import {buildRef,buildNearCashout,finishNearCalls,keepRefWarm,prepareRef,quoteRef,searchRef,tokenInfo,prepareCashout as prepareNearCashout,prepareSale as prepareNearSale,view as nearView} from './ref-swap.mjs';
import {createServer} from 'node:http';
import {createHash, randomUUID} from 'node:crypto';
import {PrivyClient} from '@privy-io/node';
import {buildSale,finishSale,buildSuiTransfer,finishSuiTransfer,quoteSwap, buildSuiSwap, finishSuiSwap, rawSignRequest, prepareSuiCashout, quoteSale, prepareSale} from './sui-swap.mjs';
import {checkSignature, signable, signForEscrow} from './base-authorization.mjs';
import {agentFor, approveAction, approveTypedData, cancelAction, checkApproval, leverageAction, moveAction,
  orderAction, post as hlPost, signAsAgent, tpslAction} from './hyperliquid.mjs';
import {prepareCashOut,finishCashOut} from './hyperliquid-cashout.mjs';

const SUI_COIN_TYPE = /^0x[0-9a-fA-F]{1,64}::[A-Za-z_][A-Za-z0-9_]*::[A-Za-z_][A-Za-z0-9_]*$/;
const POSITIVE_INTEGER = /^[1-9][0-9]{0,30}$/;
const deviceApprovals=new DeviceApprovals();
const preparedTyped=new Map();
// Sui swaps built and waiting for the user's device to approve their exact Privy request. One use each.
const preparedSwaps = new Map();

const appId = process.env.PRIVY_APP_ID;
const appSecret = process.env.PRIVY_APP_SECRET;
if (!appId || !appSecret) throw new Error('PRIVY_APP_ID and PRIVY_APP_SECRET are required');
const privy = new PrivyClient({appId, appSecret});
const port = Number(process.env.PRIVY_BRIDGE_PORT ?? 3101);
if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('invalid bridge port');

// Hyperliquid wants a fresh nonce per action: milliseconds, never repeated even within one.
let lastNonce = 0;
function nextNonce() {
  lastNonce = Math.max(Date.now(), lastNonce + 1);
  return lastNonce;
}

function send(response, status, body) {
  response.writeHead(status, {'content-type': 'application/json'});
  response.end(JSON.stringify(body));
}

async function ensureReceivingWallet(userId, chainType) {
  const externalId = `atlas_${chainType}_${createHash('sha256').update(userId).digest('hex').slice(0, 32)}`;
  for await (const wallet of privy.wallets().list({external_id: externalId})) {
    if (wallet.chain_type !== chainType || !wallet.address) {
      throw new Error('Atlas receiving wallet does not match its chain');
    }
    return wallet;
  }
  return privy.wallets().create({
    chain_type: chainType,
    owner: {user_id: userId},
    external_id: externalId,
    display_name: `Atlas ${chainType.toUpperCase()}`,
    idempotency_key: externalId,
  });
}

const server = createServer(async (request, response) => {
  // Internal notification transport. These routes never sign or move user money.
  if(request.method==='POST' && ['/internal/notifications/keys','/internal/notifications/send'].includes(request.url)){
    if(!['127.0.0.1','::1','::ffff:127.0.0.1'].includes(request.socket.remoteAddress)){
      send(response,403,{error:'Internal route'});return;
    }
    try{
      let raw='';for await(const chunk of request){raw+=chunk;if(raw.length>16384)throw new Error('Request too large');}
      const input=JSON.parse(raw||'{}');
      send(response,200,request.url.endsWith('/keys')?notificationKeys():await sendWebNotification(input));
    }catch{send(response,503,{error:'Notification transport unavailable'});}
    return;
  }
  // Public deployment readiness, never account data, credentials or a signing route.
  if(request.method==='GET'&&request.url==='/predictions/readiness'){
    try{send(response,200,await predictionAvailability());}
    catch{send(response,503,{error:'Predictions availability could not be checked. Try again.'});}
    return;
  }
  const predictionRoute=request.method==='POST'&&request.url.startsWith('/predictions/')?request.url.slice('/predictions/'.length):null;
  const verifyOnly = request.method === 'POST' && request.url === '/verify';
  const ensureWallet = request.method === 'POST' && request.url === '/wallet/ensure';
  const signAuthorization = request.method === 'POST' && request.url === '/evm/sign-authorization';
  const signEscrow = request.method === 'POST' && request.url === '/escrow/sign-authorization';
  const hlRoute = request.method === 'POST' && request.url.startsWith('/hyperliquid/') ?
    request.url.slice('/hyperliquid/'.length) : null;
  const hyperliquid = ['agent', 'approve_prepare', 'approve', 'leverage', 'order', 'tpsl', 'cancel', 'move', 'cashout_prepare', 'cashout'].includes(hlRoute);
  const refRoute = request.method === 'POST' && ['/near/ref/search','/near/ref/quote','/near/ref/prepare','/near/ref/commit','/near/ref/balance','/near/cashout/prepare','/near/cashout/commit','/near/sell/prepare','/near/sell/commit'].includes(request.url);
  const suiSale = request.method === 'POST' && ['/sui/sale/quote','/sui/sale/prepare','/sui/sale/commit'].includes(request.url);
  const suiBalanceRead=request.method==='POST' && ['/sui/balance','/sui/balances'].includes(request.url);
  const suiQuote = request.method === 'POST' && request.url === '/sui/quote';
  // The user's device approves the swap: prepare builds it and the exact request to sign; commit
  // passes the device's authorization signature to Privy (no login token is exchanged).
  const suiSwapPrepare = request.method === 'POST' && request.url === '/sui/swap/prepare';
  const suiSwapCommit = request.method === 'POST' && request.url === '/sui/swap/commit';
  const suiCashoutPrepare = request.method === 'POST' && request.url === '/sui/cashout/prepare';
  const suiCashoutCommit = request.method === 'POST' && request.url === '/sui/cashout/commit';
  if (!predictionRoute && !verifyOnly && !ensureWallet && !signAuthorization && !signEscrow && !hyperliquid && !suiQuote &&
      !suiBalanceRead && !suiSale && !refRoute && !suiCashoutPrepare && !suiCashoutCommit &&
      !suiSwapPrepare && !suiSwapCommit) {
    response.writeHead(404).end();
    return;
  }
  let body = '';
  try {
    for await (const chunk of request) {
      body += chunk;
      if (body.length > 8192) throw new Error('request too large');
    }
    const input = JSON.parse(body);
    if(refRoute && request.url === '/near/ref/search') {
      try { send(response,200,{assets:await searchRef(input.query)}); }
      catch { send(response,503,{error:'Asset details unavailable'}); }
      return;
    }
    const accessToken = input.accessToken;
    if (typeof accessToken !== 'string' || !accessToken) throw new Error('token required');
    let claims;
    try {
      claims = await privy.utils().auth().verifyAccessToken(accessToken);
    } catch {
      send(response, 401, {error: 'invalid or expired Privy access token'});
      return;
    }
    const userId = claims.userId ?? claims.user_id;
    if (typeof userId !== 'string' || !userId.startsWith('did:privy:')) {
      throw new Error('invalid identity');
    }
    const userJwt = accessToken; // Scope binding only; never passed to a wallet signing call.
    let user;
    try {
      user = await privy.users()._get(userId);
    } catch {
      send(response, 503, {error: 'Privy user lookup unavailable'});
      return;
    }
    const wallets = user.linked_accounts.filter((account) =>
      account.type === 'wallet' &&
      account.wallet_client_type === 'privy' &&
      account.connector_type === 'embedded'
    );
    const evm = wallets.find((account) => account.chain_type === 'ethereum');
    const evmWallet = evm?.address ?? null;
    const solanaWallet = wallets.find((account) => account.chain_type === 'solana')?.address ?? null;
    if(predictionRoute){
      try{send(response,200,await predictions(predictionRoute,input,{owner:evmWallet,userId,solanaWallet}));}
      catch(error){send(response,error.maybeSent?503:409,{error:error.message??'Predictions unavailable',minimumUnits:error.minimumUnits??null});}
      return;
    }
    if (verifyOnly) {
      // Bank transfers register the user with Daya by their sign-in email (and Google name).
      const emailAccount = user.linked_accounts.find((account) => account.type === 'email');
      const google = user.linked_accounts.find((account) => account.type === 'google_oauth');
      const email = emailAccount?.address ?? google?.email ?? null;
      const name = google?.name ?? null;
      send(response, 200, {userId, evmWallet, solanaWallet, email, name});
      return;
    }
    // Claiming an Atlas Link: the claimer is signed in, and the link's secret pays out its escrow,
    // only through a pinned receiver (base-authorization.mjs).
    if (signEscrow) {
      try {
        const signed = signForEscrow(input.typedData, input.secret);
        send(response, 200, {userId, escrow: signed.address, signature: signed.signature});
      } catch (error) {
        send(response, 400, {error: `invalid link payout: ${error.message}`});
      }
      return;
    }
    if(refRoute){
      try{
        const wallet=await ensureReceivingWallet(userId,'near');
        let prepared,built;
        if(request.url==='/near/cashout/prepare'){
          prepared=await prepareNearCashout({wallet,userId,userJwt,evmWallet,solanaWallet,amount:input.amount,minimumOut:input.minimumOut});
          built=await buildNearCashout({...prepared,wallet,userId,userJwt});
        }else if(request.url==='/near/sell/prepare'){
          prepared=await prepareNearSale({intentId:input.intentId,wallet,userId,userJwt,accessToken,evmWallet,solanaWallet});
          built=await buildNearCashout({...prepared,wallet,userId,userJwt});
        }else if(request.url==='/near/ref/prepare'){
          prepared=await prepareRef({intentId:input.intentId,wallet,userId,userJwt,accessToken});
          built=await buildRef({...prepared,wallet,userId,userJwt});
        }else if(request.url.endsWith('/commit')){
          const item=deviceApprovals.take({prepareId:input.prepareId,userId,wallet,signatures:input.signatures,operation:request.url});
          const signatures=await deviceSign(privy,item);
          const result=await finishNearCalls({wallet,built:item.data,signatures});
          send(response,200,{userId,address:wallet.address,...result});return;
        }else if(request.url==='/near/ref/balance'){
          send(response,200,{userId,address:wallet.address,amount:await nearView(input.token,'ft_balance_of',{account_id:wallet.address})});return;
        }else{
          const q=await quoteRef(input.token,input.amount,input.sell===true);
          send(response,200,{userId,address:wallet.address,...q,metadata:await tokenInfo(input.token)});return;
        }
        const approval=deviceApprovals.put({userId,wallet,appId,messages:built.calls.map(c=>c.message),hashFunction:'sha256',data:built,operation:request.url.replace('/prepare','/commit'),
          expires:Math.min(prepared.scope.expiresAtUnixMs,Date.now()+180000)});
        send(response,200,{userId,address:wallet.address,...prepared,...approval});
      }catch(error){send(response,409,{error:approvalFailure(error,'Swap could not complete'),txIds:error.sent??[],maybeSent:error.maybeSent===true});}
      return;
    }
    if(suiBalanceRead){
      try{
        const wallet=await ensureReceivingWallet(userId,'sui');
        if(request.url==='/sui/balances')send(response,200,{userId,address:wallet.address,result:await walletBalances(wallet.address)});
        else send(response,200,{userId,address:wallet.address,result:{totalBalance:(await coinBalance(wallet.address,input.coinType)).toString()}});
      }catch{send(response,503,{error:'Your asset balance is temporarily unavailable'});}
      return;
    }
    if(suiSale){
      try{
        const wallet=await ensureReceivingWallet(userId,'sui');
        if(request.url==='/sui/sale/quote'){
          if(!SUI_COIN_TYPE.test(input.coinType??'')||!POSITIVE_INTEGER.test(String(input.amount??'')))throw new Error('invalid sale');
          const route=await quoteSale(input.coinType,input.amount,wallet.address);
          send(response,200,{userId,address:wallet.address,amountOut:route.amountOut.toString()});
        }else if(request.url==='/sui/sale/prepare'){
          const prepared=await prepareSale({intentId:input.intentId,wallet,userId,userJwt,accessToken});
          const built=await buildSale({...prepared,wallet,userId,userJwt});
          const approval=deviceApprovals.put({userId,wallet,appId,messages:[built.message],hashFunction:'blake2b256',data:built,operation:request.url.replace('/prepare','/commit'),
            expires:Math.min(prepared.scope.expiresAtUnixMs,Date.now()+180000)});
          send(response,200,{userId,address:wallet.address,...prepared,...approval});
        }else{
          const item=deviceApprovals.take({prepareId:input.prepareId,userId,wallet,signatures:input.signatures,operation:request.url});
          const [signatureHex]=await deviceSign(privy,item);
          send(response,200,{userId,address:wallet.address,...await finishSale({wallet,built:item.data,signatureHex})});
        }
      }catch(error){send(response,409,{error:approvalFailure(error,'Sale could not complete'),maybeSent:error.maybeSent===true});}
      return;
    }
    if(suiCashoutPrepare||suiCashoutCommit){
      try{
        const wallet=await ensureReceivingWallet(userId,'sui');
        if(suiCashoutPrepare){
          const quote=await prepareSuiCashout({wallet,evmWallet,solanaWallet,amount:input.amount,minimumOut:input.minimumOut});
          const built=await buildSuiTransfer({wallet,recipient:quote.depositAddress,amount:input.amount,reserve:'20000000'});
          const approval=deviceApprovals.put({userId,wallet,appId,messages:[built.message],hashFunction:'blake2b256',data:built,operation:'/sui/cashout/commit'});
          send(response,200,{userId,address:wallet.address,...quote,...approval,cashoutId:approval.prepareId});
        }else{
          const item=deviceApprovals.take({prepareId:input.prepareId,userId,wallet,signatures:input.signatures,operation:request.url});
          const [signatureHex]=await deviceSign(privy,item);
          send(response,200,{userId,address:wallet.address,...await finishSuiTransfer({wallet,built:item.data,signatureHex})});
        }
      }catch(error){send(response,502,{error:approvalFailure(error,'Cashout unavailable'),maybeSent:error.maybeSent===true});}
      return;
    }
    if (suiSwapPrepare || suiSwapCommit) {
      let wallet;
      try {
        wallet = await ensureReceivingWallet(userId, 'sui');
      } catch {
        send(response, 503, {error: 'Privy Sui wallet unavailable; nothing was sent'});
        return;
      }
      try {
        if (suiSwapPrepare) {
          if (!SUI_COIN_TYPE.test(input.coinType ?? '') || !POSITIVE_INTEGER.test(String(input.amount ?? ''))) {
            send(response, 400, {error: 'invalid Sui swap request; nothing was sent'});
            return;
          }
          if (input.expectedWallet !== undefined && input.expectedWallet !== wallet.address) {
            throw new Error('Receiving wallet changed');
          }
          const reserve = POSITIVE_INTEGER.test(String(input.reserve ?? '')) ? input.reserve : '0';
          const built = await buildSuiSwap({wallet, coinType: input.coinType, amount: input.amount, reserve,
            minimumOut: input.minimumOut, expiresAtUnixMs: input.expiresAtUnixMs});
          // Long enough to confirm on the phone; Privy refuses the request after this too.
          const requestExpiry = Date.now() + 180_000;
          const {params, request: signable} = rawSignRequest({appId, walletId: wallet.id, message: built.message,
            requestExpiry, baseUrl: process.env.PRIVY_API_BASE_URL || 'https://api.privy.io'});
          for (const [id, prepared] of preparedSwaps) if (prepared.requestExpiry <= Date.now()) preparedSwaps.delete(id);
          if (preparedSwaps.size >= 1000) throw new Error('swap queue is full');
          const prepareId = randomUUID();
          preparedSwaps.set(prepareId, {userId, walletId: wallet.id, address: wallet.address, coinType: input.coinType,
            txBytes: built.txBytes, input: built.input, params, requestExpiry});
          send(response, 200, {userId, address: wallet.address, prepareId, request: signable,
            amountIn: built.input.toString()});
          return;
        }
        const prepared = preparedSwaps.get(input.prepareId);
        if (!prepared || prepared.userId !== userId || prepared.walletId !== wallet.id ||
            prepared.address !== wallet.address || prepared.requestExpiry <= Date.now()) {
          send(response, 409, {error: 'swap approval expired or used; nothing was sent'});
          return;
        }
        if (typeof input.signature !== 'string' || !/^[A-Za-z0-9+/=_-]{40,200}$/.test(input.signature)) {
          send(response, 400, {error: 'invalid approval; nothing was sent'});
          return;
        }
        preparedSwaps.delete(input.prepareId); // one use, even if the network response is lost
        let signatureHex;
        try {
          const signed = await privy.wallets().rawSign(prepared.walletId, {
            params: prepared.params,
            request_expiry: prepared.requestExpiry,
            authorization_context: {signatures: [input.signature]},
          });
          signatureHex = signed.signature;
        } catch (error) {
          throw new Error(`Privy refused the approval (${String(error?.message ?? error).slice(0, 120)}); nothing was signed`);
        }
        const result = await finishSuiSwap({wallet, coinType: prepared.coinType, txBytes: prepared.txBytes,
          input: prepared.input, signatureHex});
        send(response, 200, {userId, address: wallet.address, ...result});
      } catch (error) {
        const reason = String(error?.message ?? error).slice(0, 300);
        console.error(`[bridge] ${request.url} failed: ${reason}`);
        send(response, 502, {error: error?.maybeSent
          ? `Sui swap unavailable: ${reason} (may have been sent)`
          : `Sui swap unavailable: ${reason}${reason.includes('nothing was') ? '' : '; nothing was sent'}`});
      }
      return;
    }
    if(suiQuote){
      try{
        if(!SUI_COIN_TYPE.test(input.coinType??'')||!POSITIVE_INTEGER.test(String(input.amount??'')))throw new Error('invalid quote');
        const wallet=await ensureReceivingWallet(userId,'sui');
        const router=await quoteSwap(input.coinType,input.amount,wallet.address);
        send(response,200,{userId,address:wallet.address,amountOut:router.amountOut.toString()});
      }catch{send(response,503,{error:'Swap price unavailable'});}
      return;
    }
    if (ensureWallet) {
      if (!['sui', 'near'].includes(input.chainType)) {
        send(response, 400, {error: 'unsupported receiving wallet chain'});
        return;
      }
      try {
        const wallet = await ensureReceivingWallet(userId, input.chainType);
        send(response, 200, {userId, chainType: input.chainType, address: wallet.address});
      } catch {
        send(response, 503, {error: 'Privy receiving wallet unavailable'});
      }
      return;
    }
    if (typeof input.walletAddress !== 'string' ||
        evmWallet?.toLowerCase() !== input.walletAddress.toLowerCase()) {
      send(response, 403, {error: 'wallet does not belong to the signed-in user'});
      return;
    }
    // Base without gas, for a plan the user already confirmed in the app: their phone signs a
    // venue's deposit authorization or a CoW gas top-up, and nothing wider (base-authorization.mjs).
    if (signAuthorization) {
      let typedData;
      try {
        typedData = signable(input.typedData, evmWallet);
      } catch (error) {
        send(response, 400, {error: `invalid authorization: ${error.message}`});
        return;
      }
      try {
        checkSignature(typedData,input.signature,evmWallet);
        send(response,200,{userId,walletAddress:evmWallet,signature:input.signature});
      }catch(error){send(response,400,{error:error.message});}
      return;
    }
    // Hyperliquid, for the user's own account (their EVM wallet): their Atlas agent signs trades; their
    // own phone signs only the one-time approval of that agent (hyperliquid.mjs) and a cash-out to
    // their own Solana or Base wallet (hyperliquid-cashout.mjs).
    if (hyperliquid) {
      const agent = agentFor(appSecret, userId, evmWallet);
      const answer = (result) => send(response, 200, {userId, walletAddress: evmWallet, agentAddress: agent.address, result});
      try {
        if (hlRoute === 'agent') return answer(null);
        if(hlRoute==='approve_prepare'||hlRoute==='cashout_prepare'){
          for(const [id,item] of preparedTyped)if(item.expires<=Date.now())preparedTyped.delete(id);
          if(preparedTyped.size>=1000)throw new Error('approval queue full');
          const prepareId=randomUUID(),expires=Date.now()+180000;
          let data,typed;
          if(hlRoute==='approve_prepare'){
            const nonce=nextNonce();data={nonce,agentAddress:agent.address};
            typed=[approveTypedData(agent.address,nonce)];
          }else{
            const recipient={solana:solanaWallet,base:evmWallet}[input.to];
            if(!recipient)throw new Error('cash wallet unavailable');
            data=await prepareCashOut({wallet:evmWallet,recipient,to:input.to,amount:String(input.amount??''),relayKey:process.env.RELAY_API_KEY});
            typed=[data.mapping,data.send];
          }
          preparedTyped.set(prepareId,{userId,wallet:evmWallet,kind:hlRoute,data,expires});
          return answer({prepareId,expiresAtUnixMs:expires,typedData:typed.map(t=>({
            domain:t.domain,types:t.types,primaryType:t.primary_type,message:t.message}))});
        }
        if(hlRoute==='approve'||hlRoute==='cashout'){
          const prepared=preparedTyped.get(input.prepareId);
          if(!prepared||prepared.userId!==userId||prepared.wallet!==evmWallet||prepared.expires<=Date.now()||
             prepared.kind!==hlRoute+'_prepare')throw new Error('approval expired or used');
          if(!Array.isArray(input.signatures)||input.signatures.length!==(hlRoute==='approve'?1:2))throw new Error('wrong approval count');
          preparedTyped.delete(input.prepareId);
          if(hlRoute==='approve'){
            const signature=checkApproval(prepared.data.agentAddress,prepared.data.nonce,input.signatures[0],evmWallet);
            return answer(await hlPost({action:approveAction(prepared.data.agentAddress,prepared.data.nonce),nonce:prepared.data.nonce,signature}));
          }
          return answer(await finishCashOut({checked:prepared.data,wallet:evmWallet,signatures:input.signatures,
            relayKey:process.env.RELAY_API_KEY,post:hlPost}));
        }
        const nonce=nextNonce();
        const action = hlRoute === 'order' ? orderAction(input) :
          hlRoute === 'tpsl' ? tpslAction(input) :
          hlRoute === 'cancel' ? cancelAction(input) :
          hlRoute === 'move' ? moveAction({wallet: evmWallet, from: input.from, to: input.to, amount: input.amount}, nonce) :
          leverageAction(input);
        return answer(await hlPost({action, nonce, signature: signAsAgent(agent.key, action, nonce), vaultAddress: null}));
      } catch (error) {
        send(response, 502, {error: String(error?.message ?? error).slice(0, 200)});
        return;
      }
    }
  } catch {
    send(response, 400, {error: 'invalid request'});
  }
});
server.listen(port, '127.0.0.1');
keepRefWarm();
