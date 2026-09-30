import assert from 'node:assert/strict';
import {test} from 'node:test';
import {secp256k1} from '@noble/curves/secp256k1';
import {keccak_256} from '@noble/hashes/sha3';
import {
  authorizationDigest,
  checkAuthorizationSignature,
  domainSeparator,
  receiveAuthorization,
} from './base-authorization.mjs';

const key = new Uint8Array(32);
key[31] = 7;
const wallet = '0x' + Buffer.from(keccak_256(secp256k1.getPublicKey(key, false).subarray(1)).subarray(12))
  .toString('hex');

// What Layerswap returns for a gasless Base → Solana swap.
function venueTypedData(overrides = {}) {
  return {
    types: {
      EIP712Domain: [
        {name: 'name', type: 'string'},
        {name: 'version', type: 'string'},
        {name: 'chainId', type: 'uint256'},
        {name: 'verifyingContract', type: 'address'},
      ],
      ReceiveWithAuthorization: [
        {name: 'from', type: 'address'},
        {name: 'to', type: 'address'},
        {name: 'value', type: 'uint256'},
        {name: 'validAfter', type: 'uint256'},
        {name: 'validBefore', type: 'uint256'},
        {name: 'nonce', type: 'bytes32'},
      ],
    },
    primaryType: 'ReceiveWithAuthorization',
    domain: {
      name: 'USD Coin',
      version: '2',
      chainId: '8453',
      verifyingContract: '0x833589fcd6edb6e08f4c7c32d4f71b54bda02913',
      ...overrides.domain,
    },
    message: {
      from: wallet,
      to: '0x6351c235e6f7e08f80974009d01829e5a8250d62',
      value: '5000000',
      validAfter: '0',
      validBefore: '1790807789',
      nonce: '0xc5d45ec13b4d970bdd7cc5de84e783c807df2d1c645831e6d8b12717e7bdc87b',
      ...overrides.message,
    },
  };
}

test('the domain matches Base USDC on chain', () => {
  // DOMAIN_SEPARATOR() of 0x8335…2913 on Base mainnet.
  assert.equal(Buffer.from(domainSeparator()).toString('hex'),
    '02fa7265e7c5d81118673727957699e4d68f74cd74b7db77da710fe8a2c7834f');
});

test('a venue authorization is rebuilt in Privy shape and its signature checked', () => {
  const typed = receiveAuthorization(venueTypedData(), wallet.toUpperCase().replace('0X', '0x'));
  assert.equal(typed.primary_type, 'ReceiveWithAuthorization');
  assert.deepEqual(Object.keys(typed.types), ['ReceiveWithAuthorization']);
  assert.equal(typed.domain.chainId, 8453);
  assert.equal(typed.message.value, '5000000');
  const signed = secp256k1.sign(authorizationDigest(typed.message), key);
  const signature = '0x' + signed.toCompactHex() + (27 + signed.recovery).toString(16);
  checkAuthorizationSignature(typed.message, signature, wallet);
  const other = new Uint8Array(32);
  other[31] = 8;
  const forged = secp256k1.sign(authorizationDigest(typed.message), other);
  assert.throws(() => checkAuthorizationSignature(typed.message,
    '0x' + forged.toCompactHex() + (27 + forged.recovery).toString(16), wallet), /another wallet/);
});

test('anything wider than a Base USDC deposit to the pinned receiver is refused', () => {
  const refuse = (overrides, pattern) =>
    assert.throws(() => receiveAuthorization(venueTypedData(overrides), wallet), pattern);
  refuse({domain: {verifyingContract: '0x0000000000000000000000000000000000000001'}}, /Base USDC/);
  refuse({domain: {chainId: '1'}}, /Base USDC/);
  refuse({message: {from: '0x0000000000000000000000000000000000000002'}}, /this wallet/);
  refuse({message: {to: '0x0000000000000000000000000000000000000003'}}, /receiver/);
  receiveAuthorization(venueTypedData({message: {to: '0xccc88a9d1b4ed6b0eaba998850414b24f1c315be'}}), wallet);
  refuse({message: {value: '0'}}, /invalid/);
  refuse({message: {nonce: '0x12'}}, /invalid/);
  const transfer = venueTypedData();
  transfer.primaryType = 'TransferWithAuthorization';
  assert.throws(() => receiveAuthorization(transfer, wallet), /Base USDC/);
});
