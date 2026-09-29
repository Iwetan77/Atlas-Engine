import {randomBytes} from 'node:crypto';
import {secp256k1} from '@noble/curves/secp256k1';
import {keccak_256} from '@noble/hashes/sha3';

function hex(bytes) {
  return Buffer.from(bytes).toString('hex');
}

function checksumAddress(address) {
  if (!/^0x[0-9a-fA-F]{40}$/.test(address)) throw new Error('invalid Ethereum address');
  const lower = address.slice(2).toLowerCase();
  const hash = hex(keccak_256(Buffer.from(lower, 'ascii')));
  let checksummed = '0x';
  for (let i = 0; i < lower.length; i++) {
    checksummed += parseInt(hash[i], 16) >= 8 ? lower[i].toUpperCase() : lower[i];
  }
  return checksummed;
}

export function onboardingMessage(environment, address, nonce = randomBytes(16).toString('hex')) {
  const domain = environment === 'testnet'
    ? 'app.testnet.paradex.trade'
    : environment === 'prod' ? 'app.paradex.trade' : null;
  if (!domain) throw new Error('invalid Paradex environment');
  if (!/^[0-9a-fA-F]{16,64}$/.test(nonce)) throw new Error('invalid SIWE nonce');
  const chainId = environment === 'testnet' ? 11155111 : 1;
  const wallet = checksumAddress(address);
  const issuedAt = new Date().toISOString().replace(/\.\d{3}Z$/, '+00:00');
  return [
    domain + ' wants you to sign in with your Ethereum account:',
    wallet,
    '',
    'Paradex Onboarding',
    '',
    'URI: https://' + domain,
    'Version: 1',
    'Chain ID: ' + chainId,
    'Nonce: ' + nonce,
    'Issued At: ' + issuedAt,
  ].join('\n');
}

export function recoverOnboardingPublicKey(message, signature, expectedAddress) {
  if (!/^0x[0-9a-fA-F]{130}$/.test(signature)) throw new Error('invalid EVM signature');
  const sig = Buffer.from(signature.slice(2), 'hex');
  const recovery = sig[64] >= 27 ? sig[64] - 27 : sig[64];
  if (recovery > 1) throw new Error('invalid EVM recovery bit');
  const prefix = Buffer.from('\x19Ethereum Signed Message:\n' + Buffer.byteLength(message));
  const digest = keccak_256(Buffer.concat([prefix, Buffer.from(message, 'utf8')]));
  const publicKey = secp256k1.Signature.fromCompact(sig.subarray(0, 64))
    .addRecoveryBit(recovery)
    .recoverPublicKey(digest)
    .toRawBytes(false);
  const recoveredAddress = '0x' + hex(keccak_256(publicKey.subarray(1)).subarray(12));
  if (recoveredAddress.toLowerCase() !== expectedAddress.toLowerCase()) {
    throw new Error('signed wallet does not match verified Privy wallet');
  }
  return '0x' + hex(publicKey);
}
