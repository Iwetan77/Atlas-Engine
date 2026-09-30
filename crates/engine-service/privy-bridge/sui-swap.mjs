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
