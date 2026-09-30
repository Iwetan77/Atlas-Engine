// Swaps SUI in a user's own Privy Sui wallet into another Sui coin (Cetus aggregator), signed with
// Privy rawSign under the user's own login token: no server key can move the wallet on its own.
import {AggregatorClient, Env} from '@cetusprotocol/aggregator-sdk';
import {toSerializedSignature} from '@mysten/sui/cryptography';
import {SuiGrpcClient} from '@mysten/sui/grpc';
import {Ed25519PublicKey} from '@mysten/sui/keypairs/ed25519';
import {Transaction} from '@mysten/sui/transactions';
import {fromBase58, fromBase64, toBase64, toHex} from '@mysten/sui/utils';

export const SUI = '0x2::sui::SUI';
const FULLNODE = process.env.SUI_FULLNODE_URL ?? 'https://fullnode.mainnet.sui.io:443';
const SLIPPAGE = 0.01;

let grpc;
function client() {
  grpc ??= new SuiGrpcClient({network: 'mainnet', baseUrl: FULLNODE});
  return grpc;
}

async function rpc(method, params) {
  const response = await fetch(FULLNODE, {
    method: 'POST',
    headers: {'content-type': 'application/json'},
    body: JSON.stringify({jsonrpc: '2.0', id: 1, method, params}),
  });
  const body = await response.json();
  if (body.error) throw new Error(`Sui RPC ${method}: ${body.error.message ?? 'error'}`);
  return body.result;
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

export async function suiBalance(address) {
  const result = await rpc('suix_getBalance', [address, SUI]);
  return BigInt(result?.totalBalance ?? '0');
}

// Swaps up to `amount` MIST (never touching the last `reserve`, kept for gas) into `coinType`.
// `rawSign(hexMessage)` signs with the user's authorization and returns the signature hex.
export async function swapFromSui({wallet, coinType, amount, reserve, rawSign}) {
  const publicKey = walletPublicKey(wallet.public_key, wallet.address);
  const balance = await suiBalance(wallet.address);
  const spendable = balance > BigInt(reserve) ? balance - BigInt(reserve) : 0n;
  const input = BigInt(amount) < spendable ? BigInt(amount) : spendable;
  if (input <= 0n) throw new Error('no SUI to swap yet');
  const router = await quoteSwap(coinType, input, wallet.address);
  const aggregator = new AggregatorClient({signer: wallet.address, client: client(), env: Env.Mainnet});
  const txb = new Transaction();
  txb.setSender(wallet.address);
  await aggregator.fastRouterSwap({router, txb, slippage: SLIPPAGE});
  const txBytes = await txb.build({client: client()});
  const signatureHex = await rawSign(`0x${toHex(intentMessage(txBytes))}`);
  const signature = serializedSignature(signatureHex, publicKey);
  if (!(await publicKey.verifyTransaction(txBytes, signature))) throw new Error('Sui signature did not verify');
  const result = await rpc('sui_executeTransactionBlock', [
    toBase64(txBytes), [signature], {showEffects: true, showBalanceChanges: true}, 'WaitForLocalExecution',
  ]);
  const ok = result?.effects?.status?.status === 'success';
  const received = (result?.balanceChanges ?? [])
    .filter((c) => c.coinType === coinType && c.owner?.AddressOwner === wallet.address)
    .reduce((sum, c) => sum + BigInt(c.amount), 0n);
  return {digest: result?.digest, ok, error: ok ? null : result?.effects?.status?.error ?? 'swap failed',
    amountIn: input.toString(), amountOut: received.toString()};
}

// Send an exact amount of SUI to a 1Click deposit address. Privy signs only with the current
// user's JWT; the server never holds a Sui key or signs on their behalf.
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
  const result = await rpc('sui_executeTransactionBlock', [
    toBase64(txBytes), [signature], {showEffects: true, showBalanceChanges: true}, 'WaitForLocalExecution',
  ]);
  const ok = result?.effects?.status?.status === 'success';
  return {digest: result?.digest, ok, error: ok ? null : result?.effects?.status?.error ?? 'transfer failed',
    amountIn: input.toString(), recipient};
}
// A prepare step obtains the only allowed destination from 1Click for this same user's Base
// wallet. The engine persists this address before the commit step signs any Sui transaction.
export async function prepareSuiCashout({wallet, evmWallet, amount, minimumOut}) {
  if (!/^0x[0-9a-fA-F]{40}$/.test(evmWallet)) throw new Error('invalid cash wallet');
  const input = BigInt(amount);
  if (input <= 0n || BigInt(minimumOut) <= 0n) throw new Error('invalid cashout amount');
  const quoteRequest = {
    dry: false, swapType: 'EXACT_INPUT', slippageTolerance: 100,
    originAsset: 'nep141:sui.omft.near', depositType: 'ORIGIN_CHAIN',
    destinationAsset: 'nep141:base-0x833589fcd6edb6e08f4c7c32d4f71b54bda02913.omft.near',
    amount: input.toString(), recipient: evmWallet, recipientType: 'DESTINATION_CHAIN',
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