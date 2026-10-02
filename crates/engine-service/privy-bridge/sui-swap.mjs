// Swaps SUI in a user's own Privy Sui wallet into another Sui coin (Cetus aggregator), signed with
// device-approved Privy rawSign: no login token or server key can sign for this wallet.
import {PreparedSales, salePermission} from './prepared-sale.mjs';
import {AggregatorClient, Env} from '@cetusprotocol/aggregator-sdk';
import {toSerializedSignature} from '@mysten/sui/cryptography';
import {SuiGrpcClient} from '@mysten/sui/grpc';
import {Ed25519PublicKey} from '@mysten/sui/keypairs/ed25519';
import {Transaction} from '@mysten/sui/transactions';
import {fromBase58, fromBase64, toHex, normalizeStructTag} from '@mysten/sui/utils';

export const SUI = '0x2::sui::SUI';
const FULLNODE = process.env.SUI_FULLNODE_URL ?? 'https://fullnode.mainnet.sui.io:443';
const SLIPPAGE = 0.01;

const sales = new PreparedSales();
let grpc;
function client() {
  grpc ??= new SuiGrpcClient({network: 'mainnet', baseUrl: FULLNODE});
  return grpc;
}

export async function coinBalance(owner, coinType = SUI) {
  const {balance}=await client().getBalance({owner,coinType});
  if(!/^[0-9]+$/.test(balance?.balance??''))throw new Error('Sui balance unavailable');
  return BigInt(balance.balance);
}
export async function walletBalances(owner) {
  const result=[];let cursor;
  do {
    const page=await client().listBalances({owner,cursor,limit:100});
    result.push(...page.balances.map(b=>({coinType:b.coinType,totalBalance:b.balance})));
    if(!page.hasNextPage)break;
    if(!page.cursor||page.cursor===cursor)throw new Error('Sui balance pagination failed');
    cursor=page.cursor;
  }while(true);
  return result;
}
function sameCoin(a,b){try{return normalizeStructTag(a)===normalizeStructTag(b);}catch{return false;}}
export function executionResult(result){
  const tx=result.Transaction??result.FailedTransaction;
  if(!tx?.digest||typeof tx.status?.success!=='boolean')throw new Error('Sui execution outcome unavailable');
  return {digest:tx.digest,effects:{status:{status:tx.status.success?'success':'failure',error:tx.status.error?.message??'transaction failed'}},
    balanceChanges:(tx.balanceChanges??[]).map(c=>({coinType:c.coinType,owner:{AddressOwner:c.address},amount:c.amount}))};
}
async function submit(txBytes,signature){
  try { return executionResult(await client().executeTransaction({transaction:txBytes,signatures:[signature],include:{balanceChanges:true,effects:true}})); }
  catch(error){error.maybeSent=true;throw error;}
}

// Sui signs blake2b256(intent || transaction bytes); the intent for a transaction is [0, 0, 0].
export function intentMessage(txBytes) {
  const message = new Uint8Array(3 + txBytes.length);
  message.set(txBytes, 3);
  return message;
}

// Privy returns the wallet's ed25519 public key; accept hex, base64 or base58, and prove it's this
// wallet's key by deriving the address.
export function walletPublicKey(value, address) {
  if (typeof value !== 'string' || !value) throw new Error('Sui wallet has no public key');
  const raw = value.startsWith('0x') ? value.slice(2) : value;
  const candidates = [];
  if (/^[0-9a-fA-F]{64,66}$/.test(raw)) candidates.push(Uint8Array.from(Buffer.from(raw, 'hex')));
  try { candidates.push(fromBase64(value)); } catch {}
  try { candidates.push(fromBase58(value)); } catch {}
  for (let bytes of candidates) {
    // A 33-byte key carries the scheme flag first.
    if (bytes.length === 33 && bytes[0] === 0) bytes = bytes.slice(1);
    if (bytes.length !== 32) continue;
    const key = new Ed25519PublicKey(bytes);
    if (key.toSuiAddress() === address) return key;
  }
  throw new Error('Sui wallet public key does not match its address');
}

export function serializedSignature(signatureHex, publicKey) {
  const signature = Uint8Array.from(Buffer.from(signatureHex.replace(/^0x/, ''), 'hex'));
  if (signature.length !== 64) throw new Error('unexpected Sui signature length');
  return toSerializedSignature({signature, signatureScheme: 'ED25519', publicKey});
}

// What `amount` MIST of SUI buys of `coinType` right now.
export async function quoteSwap(coinType, amount, sender) {
  const aggregator = new AggregatorClient({signer: sender, client: client(), env: Env.Mainnet});
  const router = await aggregator.findRouters({from: SUI, target: coinType, amount: String(amount), byAmountIn: true});
  if (!router || router.insufficientLiquidity || router.error) throw new Error('no Sui route for this coin');
  return router;
}

export async function quoteSale(coinType, amount, sender) {
  if (!/^0x[0-9a-fA-F]{1,64}::[A-Za-z_][A-Za-z0-9_]*::[A-Za-z_][A-Za-z0-9_]*$/.test(coinType) ||
      coinType === SUI || BigInt(amount) <= 0n) throw new Error('invalid sale');
  const aggregator = new AggregatorClient({signer:sender,client:client(),env:Env.Mainnet});
  const route = await aggregator.findRouters({from:coinType,target:SUI,amount:String(amount),byAmountIn:true});
  if (!route || route.error || route.insufficientLiquidity || BigInt(route.amountOut ?? 0) <= 0n) {
    throw new Error('no sale route');
  }
  return route;
}

export async function prepareSale({intentId,wallet,userId,userJwt,accessToken}) {
  const scope = await salePermission({intentId,wallet,userId,accessToken});
  const held = await coinBalance(wallet.address,scope.coinType);
  if (BigInt(scope.amount) <= 0n || BigInt(scope.amount) > held) {
    throw new Error('the sell amount exceeds your holding');
  }
  const router = await quoteSale(scope.coinType,scope.amount,wallet.address);
  if (BigInt(router.amountOut)*99n/100n < BigInt(scope.minimumOut)) throw new Error('sale price changed');
  return {saleId:sales.put(scope,userJwt,router),scope};
}

// Scope is loaded from the confirmed engine intent and consumed once before signing.
export async function commitSale({saleId,scope,wallet,userId,userJwt,rawSign}) {
  if (scope.userId !== userId || scope.walletId !== wallet.id || scope.address !== wallet.address) {
    throw new Error('sale wallet changed');
  }
  const {scope:approved,data:router} = sales.take(saleId,scope,userJwt);
  const held = await coinBalance(wallet.address,approved.coinType);
  if (held < BigInt(approved.amount)) throw new Error('holding changed');
  const publicKey = walletPublicKey(wallet.public_key,wallet.address);
  const aggregator = new AggregatorClient({signer:wallet.address,client:client(),env:Env.Mainnet});
  const txb = new Transaction();
  txb.setSender(wallet.address);
  await aggregator.fastRouterSwap({router,txb,slippage:SLIPPAGE});
  const txBytes = await txb.build({client:client()});
  if (Date.now() >= approved.expiresAtUnixMs) throw new Error('sale expired');
  const signature = serializedSignature(await rawSign(`0x${toHex(intentMessage(txBytes))}`),publicKey);
  if (!(await publicKey.verifyTransaction(txBytes,signature))) throw new Error('signature did not verify');
  const result = await submit(txBytes,signature);
  return saleEffects(result,wallet.address);
}

// Build the exact sell before device approval; consume its confirmed scope once.
export async function buildSale({saleId,scope,wallet,userId,userJwt}) {
  if(scope.userId!==userId || scope.walletId!==wallet.id || scope.address!==wallet.address)throw new Error('sale wallet changed');
  const {scope:approved,data:router}=sales.take(saleId,scope,userJwt);
  return buildSaleTransaction({wallet,approved,router});
}
export async function buildSaleTransaction({wallet,approved,router}) {
  if(approved.address!==wallet.address || BigInt(router.amountOut)*99n/100n<BigInt(approved.minimumOut))throw new Error('sale changed');
  if(await coinBalance(wallet.address,approved.coinType)<BigInt(approved.amount))throw new Error('holding changed');
  const aggregator=new AggregatorClient({signer:wallet.address,client:client(),env:Env.Mainnet});
  const txb=new Transaction();txb.setSender(wallet.address);
  await aggregator.fastRouterSwap({router,txb,slippage:SLIPPAGE});
  const txBytes=await txb.build({client:client()});
  checkSaleGas(txb.getData().gasData.budget,await suiBalance(wallet.address));
  return {txBytes,message:`0x${toHex(intentMessage(txBytes))}`,scope:approved};
}
export async function finishSale({wallet,built,signatureHex}) {
  const result=await finishSuiSwap({wallet,coinType:SUI,txBytes:built.txBytes,input:BigInt(built.scope.amount),signatureHex});
  return result;
}
export async function buildSuiTransfer({wallet,recipient,amount,reserve}) {
  const input=BigInt(amount);
  if(input<=0n || await suiBalance(wallet.address)<input+BigInt(reserve))throw new Error('not enough SUI to send and pay gas');
  const txb=new Transaction();txb.setSender(wallet.address);
  const [coin]=txb.splitCoins(txb.gas,[input]);txb.transferObjects([coin],recipient);
  const txBytes=await txb.build({client:client()});
  return {txBytes,input,message:`0x${toHex(intentMessage(txBytes))}`,recipient};
}
export async function finishSuiTransfer({wallet,built,signatureHex}) {
  const key=walletPublicKey(wallet.public_key,wallet.address);
  const signature=serializedSignature(signatureHex,key);
  if(!await key.verifyTransaction(built.txBytes,signature))throw new Error('signature did not verify');
  const result=await submit(built.txBytes,signature);
  return {ok:result.effects.status.status==='success',digest:result.digest,amountIn:built.input.toString()};
}

// The wallet's net SUI credit includes gas. Never send an estimated output to 1Click.
export function saleEffects(result, owner) {
  const ok = result?.effects?.status?.status === 'success';
  const out = (result?.balanceChanges ?? []).filter(c => sameCoin(c.coinType,SUI) && c.owner?.AddressOwner === owner)
    .reduce((sum,c) => sum + BigInt(c.amount),0n);
  return {ok,digest:result?.digest,amountOut:out > 0n ? out.toString() : '0',error:ok ? null : 'sale did not settle'};
}

export async function suiBalance(address) {
  return coinBalance(address,SUI);
}

export function checkRecoveryQuote({minimumOut,expiresAtUnixMs,output}) {
  if(minimumOut===undefined && expiresAtUnixMs===undefined)return;
  if(!/^[1-9][0-9]*$/.test(String(minimumOut??'')) ||
     !Number.isSafeInteger(expiresAtUnixMs) || expiresAtUnixMs<=Date.now() ||
     BigInt(output)*99n/100n<BigInt(minimumOut)) {
    throw new Error('Recovery quote changed or expired; nothing was signed');
  }
}

// Swaps up to `amount` MIST (never touching the last `reserve`, kept for gas) into `coinType`.
// `rawSign(hexMessage)` signs with the user's authorization and returns the signature hex.
// Builds the swap of up to `amount` MIST (never touching the last `reserve`, kept for gas) into
// `coinType`, ready to sign. Nothing here can move money, so every failure says nothing was sent.
export async function buildSuiSwap({wallet, coinType, amount, reserve, minimumOut, expiresAtUnixMs}) {
  try {
    const balance = await suiBalance(wallet.address);
    const spendable = balance > BigInt(reserve) ? balance - BigInt(reserve) : 0n;
    const input = BigInt(amount) < spendable ? BigInt(amount) : spendable;
    if (input <= 0n) throw new Error('no SUI to swap yet');
    const router = await quoteSwap(coinType, input, wallet.address);
    checkRecoveryQuote({minimumOut,expiresAtUnixMs,output:router.amountOut.toString()});
    const aggregator = new AggregatorClient({signer: wallet.address, client: client(), env: Env.Mainnet});
    const txb = new Transaction();
    txb.setSender(wallet.address);
    await aggregator.fastRouterSwap({router, txb, slippage: SLIPPAGE});
    const txBytes = await txb.build({client: client()});
    return {txBytes, input, message: `0x${toHex(intentMessage(txBytes))}`};
  } catch (error) {
    const reason = String(error?.message ?? error);
    throw new Error(reason.includes('nothing was signed') ? reason : `${reason}; nothing was signed`);
  }
}

// Sends a built swap once its signature is in, and reads what arrived. Only a failure while
// submitting may have reached the network.
export async function finishSuiSwap({wallet, coinType, txBytes, input, signatureHex}) {
  const publicKey = walletPublicKey(wallet.public_key, wallet.address);
  const signature = serializedSignature(signatureHex, publicKey);
  if (!(await publicKey.verifyTransaction(txBytes, signature))) {
    throw new Error('Sui signature did not verify; nothing was signed');
  }
  let result;
  try {
    result = await submit(txBytes,signature);
  } catch (error) {
    error.maybeSent = true;
    throw error;
  }
  const ok = result?.effects?.status?.status === 'success';
  const received = (result?.balanceChanges ?? [])
    .filter((c) => sameCoin(c.coinType,coinType) && c.owner?.AddressOwner === wallet.address)
    .reduce((sum, c) => sum + BigInt(c.amount), 0n);
  return {digest: result?.digest, ok, error: ok ? null : result?.effects?.status?.error ?? 'swap failed',
    amountIn: input.toString(), amountOut: received.toString()};
}

// Swaps up to `amount` MIST into `coinType`. `rawSign(hexMessage)` signs with the user's authorization
// and returns the signature hex.
export async function swapFromSui({wallet, coinType, amount, reserve, rawSign, minimumOut, expiresAtUnixMs}) {
  const {txBytes, input, message} = await buildSuiSwap({wallet, coinType, amount, reserve, minimumOut, expiresAtUnixMs});
  const signatureHex = await rawSign(message);
  return finishSuiSwap({wallet, coinType, txBytes, input, signatureHex});
}

// The exact Privy raw_sign request for `message`, as Privy's server SDK sends it: the user's device
// signs this with their own authorization key, and the bridge passes that signature along.
export function rawSignRequest({appId, walletId, message, requestExpiry, baseUrl = 'https://api.privy.io'}) {
  const params = {bytes: message, encoding: 'hex', hash_function: 'blake2b256'};
  return {
    params,
    request: {
      version: 1,
      method: 'POST',
      url: `${baseUrl}/v1/wallets/${walletId}/raw_sign`,
      body: {params},
      headers: {'privy-app-id': appId, 'privy-request-expiry': String(requestExpiry)},
    },
  };
}

export async function transferSui({wallet, recipient, amount, reserve, rawSign}) {
  if (!/^0x[0-9a-fA-F]{64}$/.test(recipient)) throw new Error('invalid Sui recipient');
  const input = BigInt(amount);
  if (input <= 0n) throw new Error('invalid SUI transfer amount');
  const balance = await suiBalance(wallet.address);
  if (balance < input + BigInt(reserve)) throw new Error('not enough SUI to send and pay gas');
  const publicKey = walletPublicKey(wallet.public_key, wallet.address);
  const txb = new Transaction();
  txb.setSender(wallet.address);
  const [coin] = txb.splitCoins(txb.gas, [input]);
  txb.transferObjects([coin], recipient);
  const txBytes = await txb.build({client: client()});
  const signatureHex = await rawSign(`0x${toHex(intentMessage(txBytes))}`);
  const signature = serializedSignature(signatureHex, publicKey);
  if (!(await publicKey.verifyTransaction(txBytes, signature))) throw new Error('Sui signature did not verify');
  const result = await submit(txBytes,signature);
  const ok = result?.effects?.status?.status === 'success';
  return {digest: result?.digest, ok, error: ok ? null : result?.effects?.status?.error ?? 'transfer failed',
    amountIn: input.toString(), recipient};
}
// A prepare step obtains the only allowed destination from 1Click for this same user's Base
// wallet. The engine persists this address before the commit step signs any Sui transaction.
export async function prepareSuiCashout({wallet, evmWallet, solanaWallet, amount, minimumOut}) {
  if (!solanaWallet && !/^0x[0-9a-fA-F]{40}$/.test(evmWallet)) throw new Error('invalid cash wallet');
  const input = BigInt(amount);
  if (input <= 0n || BigInt(minimumOut) <= 0n) throw new Error('invalid cashout amount');
  const quoteRequest = {
    dry: false, swapType: 'EXACT_INPUT', slippageTolerance: 100,
    originAsset: 'nep141:sui.omft.near', depositType: 'ORIGIN_CHAIN',
    destinationAsset: solanaWallet ? 'nep141:sol-5ce3bf3a31af18be40ba30f721101b4341690186.omft.near'
      : 'nep141:base-0x833589fcd6edb6e08f4c7c32d4f71b54bda02913.omft.near',
    amount: input.toString(), recipient: solanaWallet ?? evmWallet, recipientType: 'DESTINATION_CHAIN',
    refundTo: wallet.address, refundType: 'ORIGIN_CHAIN',
    deadline: new Date(Date.now() + 240_000).toISOString(),
  };
  const key = process.env.NEAR_INTENTS_API_KEY;
  const response = await fetch('https://1click.chaindefuser.com/v0/quote', {
    method: 'POST',
    headers: {'content-type': 'application/json', ...(key ? {'X-API-Key': key} : {})},
    body: JSON.stringify(quoteRequest),
    signal: AbortSignal.timeout(20_000),
  });
  if (!response.ok) throw new Error('cashout route unavailable');
  const quote = (await response.json()).quote;
  if (quote?.amountIn !== input.toString() ||
      BigInt(quote.minAmountOut ?? quote.amountOut ?? 0) < BigInt(minimumOut) ||
      !/^0x[0-9a-fA-F]{64}$/.test(quote.depositAddress ?? '') ||
      quote.depositMemo != null) throw new Error('invalid cashout deposit route');
  return {depositAddress: quote.depositAddress, amountOut: quote.amountOut};
}
// The sale produces SUI; its cash-out keeps 0.02 SUI. Before approving the swap, existing SUI must cover its real budget.
export function checkSaleGas(budget,held) {
  if(!/^[1-9][0-9]*$/.test(String(budget??'')) || BigInt(held)<BigInt(budget))throw new Error('not enough for the transaction gas budget');
}
