// A gasless Base USDC deposit: the user's wallet signs one EIP-3009 ReceiveWithAuthorization and the
// venue's relayer submits it, paying the gas. Only the named receiver can redeem it, for exactly the
// signed amount, so the bridge signs nothing wider: this USDC contract, this chain, the user's own
// wallet, a pinned receiver.
import {secp256k1} from '@noble/curves/secp256k1';
import {keccak_256} from '@noble/hashes/sha3';

export const BASE_USDC = '0x833589fcd6edb6e08f4c7c32d4f71b54bda02913';
// The only accounts that may redeem it: Layerswap's gasless receiver and Relay's receiver on Base.
export const RECEIVERS = new Set([
  '0x6351c235e6f7e08f80974009d01829e5a8250d62',
  '0xccc88a9d1b4ed6b0eaba998850414b24f1c315be',
]);

const ADDRESS = /^0x[0-9a-fA-F]{40}$/;
const UINT = /^(0|[1-9][0-9]{0,30})$/;
const BYTES32 = /^0x[0-9a-fA-F]{64}$/;
const FIELDS = [
  {name: 'from', type: 'address'},
  {name: 'to', type: 'address'},
  {name: 'value', type: 'uint256'},
  {name: 'validAfter', type: 'uint256'},
  {name: 'validBefore', type: 'uint256'},
  {name: 'nonce', type: 'bytes32'},
];

// Checks the venue's typed data and rebuilds it from scratch in Privy's shape, so nothing beyond the
// checked fields is ever signed.
export function receiveAuthorization(typedData, wallet) {
  const domain = typedData?.domain ?? {};
  const message = typedData?.message ?? {};
  const fields = typedData?.types?.ReceiveWithAuthorization;
  const same = (a, b) => typeof a === 'string' && a.toLowerCase() === b.toLowerCase();
  if (typedData?.primaryType !== 'ReceiveWithAuthorization' ||
      JSON.stringify(fields) !== JSON.stringify(FIELDS) ||
      domain.name !== 'USD Coin' || domain.version !== '2' || String(domain.chainId) !== '8453' ||
      !same(domain.verifyingContract, BASE_USDC)) {
    throw new Error('not a Base USDC authorization');
  }
  if (!ADDRESS.test(wallet) || !same(message.from, wallet)) throw new Error('not from this wallet');
  if (!RECEIVERS.has(String(message.to).toLowerCase())) throw new Error('unknown receiver');
  const value = String(message.value);
  const validAfter = String(message.validAfter);
  const validBefore = String(message.validBefore);
  if (!UINT.test(value) || value === '0' || !UINT.test(validAfter) || !UINT.test(validBefore) ||
      !BYTES32.test(message.nonce)) {
    throw new Error('invalid authorization');
  }
  return {
    domain: {name: 'USD Coin', version: '2', chainId: 8453, verifyingContract: BASE_USDC},
    types: {ReceiveWithAuthorization: FIELDS},
    primary_type: 'ReceiveWithAuthorization',
    message: {
      from: wallet.toLowerCase(),
      to: message.to.toLowerCase(),
      value,
      validAfter,
      validBefore,
      nonce: message.nonce.toLowerCase(),
    },
  };
}

const word = (value) => BigInt(value).toString(16).padStart(64, '0');
const utf8Hash = (text) => Buffer.from(keccak_256(Buffer.from(text, 'utf8'))).toString('hex');

export function domainSeparator() {
  const type = utf8Hash('EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)');
  return keccak_256(Buffer.from(
    type + utf8Hash('USD Coin') + utf8Hash('2') + word(8453) + word(BASE_USDC), 'hex'));
}

// The EIP-712 digest the wallet signs for a rebuilt authorization.
export function authorizationDigest(message) {
  const type = utf8Hash('ReceiveWithAuthorization(address from,address to,uint256 value,' +
    'uint256 validAfter,uint256 validBefore,bytes32 nonce)');
  const struct = keccak_256(Buffer.from(
    type + word(message.from) + word(message.to) + word(message.value) + word(message.validAfter) +
    word(message.validBefore) + message.nonce.slice(2), 'hex'));
  return keccak_256(Buffer.concat([Buffer.from([0x19, 0x01]), domainSeparator(), struct]));
}

// Throws unless `signature` is `wallet` signing this exact authorization.
export function checkAuthorizationSignature(message, signature, wallet) {
  if (!/^0x[0-9a-fA-F]{130}$/.test(signature)) throw new Error('invalid EVM signature');
  const sig = Buffer.from(signature.slice(2), 'hex');
  const recovery = sig[64] >= 27 ? sig[64] - 27 : sig[64];
  if (recovery > 1) throw new Error('invalid EVM recovery bit');
  const publicKey = secp256k1.Signature.fromCompact(sig.subarray(0, 64))
    .addRecoveryBit(recovery)
    .recoverPublicKey(authorizationDigest(message))
    .toRawBytes(false);
  const signer = '0x' + Buffer.from(keccak_256(publicKey.subarray(1)).subarray(12)).toString('hex');
  if (signer !== wallet.toLowerCase()) throw new Error('signed by another wallet');
}
