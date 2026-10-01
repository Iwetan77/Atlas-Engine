// What the user's wallet may sign on Base in place of a transaction, and nothing wider. Each shape
// is checked and rebuilt from scratch in Privy's typed-data format, so no unchecked field is signed:
// - ReceiveWithAuthorization (EIP-3009): a gasless USDC deposit that only a pinned venue receiver
//   (Relay, Layerswap) can redeem, for exactly the signed amount; the venue pays the gas.
// - Permit (EIP-2612): CoW's vault relayer may take at most a gas top-up's worth of USDC.
// - Order: a CoW sale of at most that much USDC for ETH, paid to the wallet itself.
import {secp256k1} from '@noble/curves/secp256k1';
import {keccak_256} from '@noble/hashes/sha3';

export const BASE_USDC = '0x833589fcd6edb6e08f4c7c32d4f71b54bda02913';
// The only accounts that may redeem a deposit authorization: Layerswap's gasless receiver and
// Relay's receiver on Base.
export const RECEIVERS = new Set([
  '0x6351c235e6f7e08f80974009d01829e5a8250d62',
  '0xccc88a9d1b4ed6b0eaba998850414b24f1c315be',
]);
// CoW Protocol on Base, and how it names native ETH.
export const COW_VAULT_RELAYER = '0xc92e8bdf79f0507f65a392b0ab4667716bfe0110';
export const COW_SETTLEMENT = '0x9008d19f58aabd9ed0d60971565aa8510560ab41';
const NATIVE_ETH = '0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee';
// A gas top-up is $0.50; nothing CoW is asked to take may exceed $2 or live past two hours.
const TOP_UP_MAX = 2_000_000n;
const LIFETIME_SECS = 2 * 60 * 60;

const ADDRESS = /^0x[0-9a-fA-F]{40}$/;
const UINT = /^(0|[1-9][0-9]{0,77})$/;
const BYTES32 = /^0x[0-9a-fA-F]{64}$/;

const USDC_DOMAIN = {name: 'USD Coin', version: '2', chainId: 8453, verifyingContract: BASE_USDC};
const COW_DOMAIN = {name: 'Gnosis Protocol', version: 'v2', chainId: 8453, verifyingContract: COW_SETTLEMENT};
const TYPES = {
  ReceiveWithAuthorization: [
    {name: 'from', type: 'address'},
    {name: 'to', type: 'address'},
    {name: 'value', type: 'uint256'},
    {name: 'validAfter', type: 'uint256'},
    {name: 'validBefore', type: 'uint256'},
    {name: 'nonce', type: 'bytes32'},
  ],
  Permit: [
    {name: 'owner', type: 'address'},
    {name: 'spender', type: 'address'},
    {name: 'value', type: 'uint256'},
    {name: 'nonce', type: 'uint256'},
    {name: 'deadline', type: 'uint256'},
  ],
  Order: [
    {name: 'sellToken', type: 'address'},
    {name: 'buyToken', type: 'address'},
    {name: 'receiver', type: 'address'},
    {name: 'sellAmount', type: 'uint256'},
    {name: 'buyAmount', type: 'uint256'},
    {name: 'validTo', type: 'uint32'},
    {name: 'appData', type: 'bytes32'},
    {name: 'feeAmount', type: 'uint256'},
    {name: 'kind', type: 'string'},
    {name: 'partiallyFillable', type: 'bool'},
    {name: 'sellTokenBalance', type: 'string'},
    {name: 'buyTokenBalance', type: 'string'},
  ],
};

const lower = (v) => String(v).toLowerCase();
const uint = (v) => {
  const text = String(v);
  if (!UINT.test(text)) throw new Error('invalid number');
  return text;
};
function sameDomain(domain, expected) {
  return domain?.name === expected.name && domain?.version === expected.version &&
    String(domain?.chainId) === String(expected.chainId) &&
    lower(domain?.verifyingContract) === expected.verifyingContract;
}
function within(deadline, now) {
  const at = Number(uint(deadline));
  if (!(at > now && at <= now + LIFETIME_SECS)) throw new Error('expires too late or already expired');
}
function topUp(value) {
  const amount = BigInt(uint(value));
  if (amount === 0n || amount > TOP_UP_MAX) throw new Error('more than a gas top-up');
  return amount.toString();
}

// Each shape: its domain, and the message rebuilt from checked fields (throws otherwise).
const SHAPES = {
  ReceiveWithAuthorization: {
    domain: USDC_DOMAIN,
    rebuild(m, wallet) {
      if (lower(m.from) !== wallet) throw new Error('not from this wallet');
      if (!RECEIVERS.has(lower(m.to))) throw new Error('unknown receiver');
      if (uint(m.value) === '0' || !BYTES32.test(m.nonce)) throw new Error('invalid authorization');
      return {from: wallet, to: lower(m.to), value: uint(m.value), validAfter: uint(m.validAfter),
        validBefore: uint(m.validBefore), nonce: lower(m.nonce)};
    },
  },
  Permit: {
    domain: USDC_DOMAIN,
    rebuild(m, wallet, now) {
      if (lower(m.owner) !== wallet) throw new Error('not from this wallet');
      if (lower(m.spender) !== COW_VAULT_RELAYER) throw new Error('unknown spender');
      within(m.deadline, now);
      return {owner: wallet, spender: COW_VAULT_RELAYER, value: topUp(m.value), nonce: uint(m.nonce),
        deadline: uint(m.deadline)};
    },
  },
  Order: {
    domain: COW_DOMAIN,
    rebuild(m, wallet, now) {
      if (lower(m.sellToken) !== BASE_USDC || lower(m.buyToken) !== NATIVE_ETH) {
        throw new Error('not a USDC to ETH order');
      }
      if (lower(m.receiver) !== wallet) throw new Error('not paid to this wallet');
      if (m.kind !== 'sell' || m.partiallyFillable !== false || String(m.feeAmount) !== '0' ||
          m.sellTokenBalance !== 'erc20' || m.buyTokenBalance !== 'erc20' || !BYTES32.test(m.appData) ||
          uint(m.buyAmount) === '0') {
        throw new Error('not a plain sell order');
      }
      within(m.validTo, now);
      return {sellToken: BASE_USDC, buyToken: NATIVE_ETH, receiver: wallet, sellAmount: topUp(m.sellAmount),
        buyAmount: uint(m.buyAmount), validTo: Number(m.validTo), appData: lower(m.appData), feeAmount: '0',
        kind: 'sell', partiallyFillable: false, sellTokenBalance: 'erc20', buyTokenBalance: 'erc20'};
    },
  },
};

// Checks the engine's typed data against the shapes above and rebuilds it in Privy's format.
export function signable(typedData, wallet, now = Math.floor(Date.now() / 1000)) {
  const primary = typedData?.primaryType;
  const shape = Object.hasOwn(SHAPES, primary) ? SHAPES[primary] : null;
  if (!shape || JSON.stringify(typedData?.types?.[primary]) !== JSON.stringify(TYPES[primary]) ||
      !sameDomain(typedData?.domain, shape.domain)) {
    throw new Error('not something Atlas signs');
  }
  if (!ADDRESS.test(wallet ?? '')) throw new Error('invalid wallet');
  return {
    domain: shape.domain,
    types: {[primary]: TYPES[primary]},
    primary_type: primary,
    message: shape.rebuild(typedData.message ?? {}, wallet.toLowerCase(), now),
  };
}

const word = (value) => BigInt(value).toString(16).padStart(64, '0');
const utf8Hash = (text) => Buffer.from(keccak_256(Buffer.from(text, 'utf8'))).toString('hex');

export function domainSeparator(domain) {
  const type = utf8Hash('EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)');
  return keccak_256(Buffer.from(type + utf8Hash(domain.name) + utf8Hash(domain.version) +
    word(domain.chainId) + word(domain.verifyingContract), 'hex'));
}

// The EIP-712 digest the wallet signs for rebuilt (Privy-format) typed data.
export function typedDigest(typed) {
  const fields = TYPES[typed.primary_type];
  const type = utf8Hash(`${typed.primary_type}(${fields.map((f) => `${f.type} ${f.name}`).join(',')})`);
  const encoded = fields.map(({name, type: kind}) => {
    const value = typed.message[name];
    if (kind === 'string') return utf8Hash(value);
    if (kind === 'bytes32') return value.slice(2);
    if (kind === 'bool') return word(value ? 1 : 0);
    return word(value);
  });
  const struct = keccak_256(Buffer.from(type + encoded.join(''), 'hex'));
  return keccak_256(Buffer.concat([Buffer.from([0x19, 0x01]), domainSeparator(typed.domain), struct]));
}

// Throws unless `signature` is `wallet` signing exactly `typed`.
export function checkSignature(typed, signature, wallet) {
  if (!/^0x[0-9a-fA-F]{130}$/.test(signature)) throw new Error('invalid EVM signature');
  const sig = Buffer.from(signature.slice(2), 'hex');
  const recovery = sig[64] >= 27 ? sig[64] - 27 : sig[64];
  if (recovery > 1) throw new Error('invalid EVM recovery bit');
  const publicKey = secp256k1.Signature.fromCompact(sig.subarray(0, 64))
    .addRecoveryBit(recovery)
    .recoverPublicKey(typedDigest(typed))
    .toRawBytes(false);
  const signer = '0x' + Buffer.from(keccak_256(publicKey.subarray(1)).subarray(12)).toString('hex');
  if (signer !== wallet.toLowerCase()) throw new Error('signed by another wallet');
}

// An Atlas Link's escrow: the key is the link's secret (32 bytes, 0x-hex), made on the sender's phone
// and carried only in the link. Returns the key and its address.
export function escrowKey(secret) {
  if (!/^0x[0-9a-fA-F]{64}$/.test(secret ?? '')) throw new Error('invalid link secret');
  const key = Buffer.from(secret.slice(2), 'hex');
  if (!secp256k1.utils.isValidPrivateKey(key)) throw new Error('invalid link secret');
  const address = '0x' + Buffer.from(keccak_256(secp256k1.getPublicKey(key, false).subarray(1)).subarray(12))
    .toString('hex');
  return {key, address};
}

// Signs for a link's escrow, and only a deposit authorization from it to a pinned receiver (Relay
// paying the money out to whoever claimed it): nothing else can be signed with a link's secret.
export function signForEscrow(typedData, secret, now = Math.floor(Date.now() / 1000)) {
  const {key, address} = escrowKey(secret);
  const typed = signable(typedData, address, now);
  if (typed.primary_type !== 'ReceiveWithAuthorization') {
    throw new Error('a link only pays out through a pinned receiver');
  }
  const signed = secp256k1.sign(typedDigest(typed), key);
  return {address, signature: '0x' + signed.toCompactHex() + (27 + signed.recovery).toString(16)};
}
