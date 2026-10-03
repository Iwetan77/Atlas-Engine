// Hyperliquid perps for an Atlas user. The account is the user's own EVM wallet. Trades are signed by
// an agent key Atlas derives per user, which the user approves once with their own wallet: an agent
// can place and cancel orders, set leverage and move margin between the account's own dexes, never
// withdraw or pay anyone else. Formats were checked against the live exchange (it recovered exactly
// the signing addresses).
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

// A take-profit and/or stop-loss on the whole position ("position TP/SL"): size 0 follows the
// position as it changes, and both close it at market once the price trades through the trigger.
// `isBuy` is the closing side (a long closes by selling); `price` is the worst fill allowed.
export function tpslAction({asset, isBuy, orders}) {
  if (!ASSET(asset) || typeof isBuy !== 'boolean' || !Array.isArray(orders) || orders.length < 1 ||
      orders.length > 2 || new Set(orders.map((o) => o?.tpsl)).size !== orders.length) {
    throw new Error('invalid tpsl');
  }
  return {
    type: 'order',
    orders: orders.map(({tpsl, trigger, price}) => {
      if (tpsl !== 'tp' && tpsl !== 'sl') throw new Error('invalid tpsl');
      const triggerPx = wire(trigger);
      const p = wire(price);
      if (Number(triggerPx) <= 0 || Number(p) <= 0) throw new Error('invalid tpsl');
      // Field order matters: it's what gets hashed.
      return {a: asset, b: isBuy, p, s: '0', r: true, t: {trigger: {isMarket: true, triggerPx, tpsl}}};
    }),
    grouping: 'positionTpsl',
  };
}

// Cancels resting orders (a position's TP/SL) by id.
export function cancelAction({asset, oids}) {
  if (!ASSET(asset) || !Array.isArray(oids) || oids.length < 1 || oids.length > 10 ||
      !oids.every((o) => Number.isSafeInteger(o) && o > 0)) {
    throw new Error('invalid cancel');
  }
  return {type: 'cancel', cancels: oids.map((o) => ({a: asset, o}))};
}

// Cross margin, or margin per position for markets that only allow that.
export function leverageAction({asset, leverage, isolated = false}) {
  if (!ASSET(asset) || !Number.isInteger(leverage) || leverage < 1 || leverage > 100 ||
      typeof isolated !== 'boolean') {
    throw new Error('invalid leverage');
  }
  return {type: 'updateLeverage', asset, isCross: !isolated, leverage};
}

// The dexes margin may move between: Hyperliquid's own perps ("") and the stock dex.
const DEXES = new Set(['', 'xyz']);
const USDC_TOKEN = 'USDC:0x6d1e7cde53ba9467b783cb7c530ce054';

// USDC (units, 6 decimals) from one of the account's dexes to another. Hyperliquid only lets an
// agent send to the same account, and the destination here is always the user's own wallet.
export function moveAction({wallet, from, to, amount}, nonce) {
  if (!DEXES.has(from) || !DEXES.has(to) || from === to || !/^0x[0-9a-fA-F]{40}$/.test(wallet ?? '') ||
      !/^[1-9][0-9]{0,15}$/.test(String(amount ?? ''))) {
    throw new Error('invalid margin move');
  }
  const units = String(amount).padStart(7, '0');
  // Field order matters: it's what gets hashed.
  return {type: 'agentSendAsset', destination: wallet.toLowerCase(), sourceDex: from, destinationDex: to,
    token: USDC_TOKEN, amount: `${units.slice(0, -6)}.${units.slice(-6)}`, fromSubAccount: '', nonce};
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
