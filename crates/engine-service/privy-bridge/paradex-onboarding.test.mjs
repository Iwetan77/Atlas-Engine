import assert from 'node:assert/strict';
import {test} from 'node:test';
import {secp256k1} from '@noble/curves/secp256k1';
import {keccak_256} from '@noble/hashes/sha3';
import {authMessage, onboardingMessage, recoverOnboardingPublicKey} from './paradex-onboarding.mjs';

test('testnet onboarding signs only the fixed Paradex SIWE message', () => {
  const key = new Uint8Array(32);
  key[31] = 1;
  const publicKey = secp256k1.getPublicKey(key, false);
  const address = '0x' + Buffer.from(keccak_256(publicKey.subarray(1)).subarray(12)).toString('hex');
  const message = onboardingMessage('testnet', address, '0123456789abcdef0123456789abcdef');
  assert.match(message, /^app\.testnet\.paradex\.trade wants you to sign in/);
  assert.match(message, /\nParadex Onboarding\n/);
  assert.match(message, /\nChain ID: 11155111\n/);
  assert.match(message, /\nIssued At: \d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\+00:00$/);
  const prefix = Buffer.from('\x19Ethereum Signed Message:\n' + Buffer.byteLength(message));
  const digest = keccak_256(Buffer.concat([prefix, Buffer.from(message)]));
  const signature = secp256k1.sign(digest, key);
  const encoded = '0x' + Buffer.from(signature.toCompactRawBytes()).toString('hex') +
    (27 + signature.recovery).toString(16);
  assert.equal(recoverOnboardingPublicKey(message, encoded, address),
    '0x' + Buffer.from(publicKey).toString('hex'));
  assert.throws(() => recoverOnboardingPublicKey(message, encoded,
    '0x0000000000000000000000000000000000000001'));
});

test('Paradex account authentication expires and is wallet bound', () => {
  const address = '0x0000000000000000000000000000000000000001';
  const message = authMessage('testnet', address, '0123456789abcdef0123456789abcdef');
  assert.match(message, /\nParadex Auth\n/);
  assert.match(message, /\nIssued At: .+\nExpiration Time: .+$/);
  assert.match(message, /\nChain ID: 11155111\n/);
});
test('invalid inputs fail before any wallet signature', () => {
  assert.throws(() => onboardingMessage('unknown', '0x0000000000000000000000000000000000000001'));
  assert.throws(() => onboardingMessage('prod', 'not-an-address'));
  assert.throws(() => onboardingMessage('prod',
    '0x0000000000000000000000000000000000000001', 'bad'));
});
