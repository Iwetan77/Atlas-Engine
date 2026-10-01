// Hyperliquid perps for an Atlas user. The account is the user's own EVM wallet. Trades are signed by
// an agent key Atlas derives per user, which the user approves once with their own wallet: an agent
// can place and cancel orders and set leverage, never withdraw or transfer. Formats were checked
// against the live exchange (it recovered exactly the signing addresses).
import {createHmac} from 'node:crypto';
import {encode} from '@msgpack/msgpack';
import {secp256k1} from '@noble/curves/secp256k1';
import {keccak_256} from '@noble/hashes/sha3';

export const EXCHANGE = 'https://api.hyperliquid.xyz/exchange';
// User-signed actions carry this chain id (Hyperliquid's own convention).
const SIGNATURE_CHAIN_ID = '0x66eee';
const AGENT_NAME = 'atlas';

const hex = (bytes) => Buffer.from(bytes).toString('hex');
const utf8Hash = (text) => hex(keccak_256(Buffer.from(text, 'utf8')));
const word = (value) => BigInt(value).toString(16).padStart(64, '0');
const addressOf = (key) => '0x' + hex(keccak_256(secp256k1.getPublicKey(key, false).subarray(1)).subarray(12));

// The user's agent: derived from the app secret, so it's the same key every time and nothing is stored.
export function agentFor(secret, userId, wallet) {
  if (!secret) throw new Error('agent secret missing');
  for (let round = 0; ; round += 1) {
    const key = createHmac('sha256', secret)
      .update(`atlas-hyperliquid-agent|${round}|${userId}|${wallet.toLowerCase()}`)
      .digest();
    if (secp256k1.utils.isValidPrivateKey(key)) return {key, address: addressOf(key)};
  }
}

function domainSeparator(name, chainId) {
  return keccak_256(Buffer.from(
    utf8Hash('EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)') +
    utf8Hash(name) + utf8Hash('1') + word(chainId) + word(0), 'hex'));
}

function digest(domain, structHash) {
  return keccak_256(Buffer.concat([Buffer.from([0x19, 0x01]), domain, structHash]));
}

function rsv(signed) {
  return {
    r: '0x' + signed.r.toString(16).padStart(64, '0'),
    s: '0x' + signed.s.toString(16).padStart(64, '0'),
    v: 27 + signed.recovery,
  };
}

// The hash an agent signs for a trading action: msgpack(action) ‖ nonce (u64, big-endian) ‖ no vault.
export function actionHash(action, nonce) {
  const nonceBytes = Buffer.alloc(8);
  nonceBytes.writeBigUInt64BE(BigInt(nonce));
  return keccak_256(Buffer.concat([Buffer.from(encode(action)), nonceBytes, Buffer.from([0])]));
}

export function agentDigest(action, nonce) {
  const struct = keccak_256(Buffer.from(
    utf8Hash('Agent(string source,bytes32 connectionId)') + utf8Hash('a') + hex(actionHash(action, nonce)), 'hex'));
  return digest(domainSeparator('Exchange', 1337), struct);
}

export function signAsAgent(key, action, nonce) {
  return rsv(secp256k1.sign(agentDigest(action, nonce), key));
}

// Prices and sizes on the wire: plain decimals, no trailing zeros ("5.30" → "5.3").
export function wire(value) {
  const text = String(value);
  if (!/^(0|[1-9][0-9]*)(\.[0-9]{1,8})?$/.test(text)) throw new Error('invalid number');
  return text.includes('.') ? text.replace(/0+$/, '').replace(/\.$/, '') : text;
}

const ASSET = (a) => Number.isInteger(a) && a >= 0 && a < 1_000_000;

// An immediate-or-cancel order: a market order with a price limit.
export function orderAction({asset, isBuy, price, size, reduceOnly}) {
  if (!ASSET(asset) || typeof isBuy !== 'boolean' || typeof reduceOnly !== 'boolean') {
    throw new Error('invalid order');
  }
  const s = wire(size);
  if (Number(s) <= 0) throw new Error('invalid order');
  // Field order matters: it's what gets hashed.
  return {
    type: 'order',
    orders: [{a: asset, b: isBuy, p: wire(price), s, r: reduceOnly, t: {limit: {tif: 'Ioc'}}}],
    grouping: 'na',
  };
}

export function leverageAction({asset, leverage}) {
  if (!ASSET(asset) || !Number.isInteger(leverage) || leverage < 1 || leverage > 100) {
    throw new Error('invalid leverage');
  }
  return {type: 'updateLeverage', asset, isCross: true, leverage};
}

// The one thing the user's own wallet signs here: approving their Atlas agent, and no other key.
export function approveTypedData(agentAddress, nonce) {
  return {
    domain: {name: 'HyperliquidSignTransaction', version: '1', chainId: Number(SIGNATURE_CHAIN_ID),
      verifyingContract: '0x0000000000000000000000000000000000000000'},
    types: {'HyperliquidTransaction:ApproveAgent': [
      {name: 'hyperliquidChain', type: 'string'},
      {name: 'agentAddress', type: 'address'},
      {name: 'agentName', type: 'string'},
      {name: 'nonce', type: 'uint64'},
    ]},
    primary_type: 'HyperliquidTransaction:ApproveAgent',
    message: {hyperliquidChain: 'Mainnet', agentAddress, agentName: AGENT_NAME, nonce},
  };
}

export function approveAction(agentAddress, nonce) {
  return {type: 'approveAgent', signatureChainId: SIGNATURE_CHAIN_ID, hyperliquidChain: 'Mainnet',
    agentAddress, agentName: AGENT_NAME, nonce};
}

export function approveDigest(agentAddress, nonce) {
  const struct = keccak_256(Buffer.from(
    utf8Hash('HyperliquidTransaction:ApproveAgent(string hyperliquidChain,address agentAddress,string agentName,uint64 nonce)') +
    utf8Hash('Mainnet') + word(agentAddress) + utf8Hash(AGENT_NAME) + word(nonce), 'hex'));
  return digest(domainSeparator('HyperliquidSignTransaction', Number(SIGNATURE_CHAIN_ID)), struct);
}

// Throws unless `signature` (0x r s v) is `wallet` approving exactly this agent.
export function checkApproval(agentAddress, nonce, signature, wallet) {
  if (!/^0x[0-9a-fA-F]{130}$/.test(signature)) throw new Error('invalid signature');
  const sig = Buffer.from(signature.slice(2), 'hex');
  const recovery = sig[64] >= 27 ? sig[64] - 27 : sig[64];
  const publicKey = secp256k1.Signature.fromCompact(sig.subarray(0, 64)).addRecoveryBit(recovery)
    .recoverPublicKey(approveDigest(agentAddress, nonce)).toRawBytes(false);
  const signer = '0x' + hex(keccak_256(publicKey.subarray(1)).subarray(12));
  if (signer !== wallet.toLowerCase()) throw new Error('signed by another wallet');
  return {r: '0x' + signature.slice(2, 66), s: '0x' + signature.slice(66, 130), v: recovery + 27};
}

export async function post(body) {
  const response = await fetch(EXCHANGE, {
    method: 'POST',
    headers: {'content-type': 'application/json'},
    body: JSON.stringify(body),
  });
  const text = await response.text();
  try {
    return JSON.parse(text);
  } catch {
    return {status: 'err', response: text.slice(0, 200)};
  }
}
