import assert from 'node:assert/strict';
import {test} from 'node:test';
import {secp256k1} from '@noble/curves/secp256k1';
import {keccak_256} from '@noble/hashes/sha3';
import {checkSignature, domainSeparator, escrowKey, signable, signForEscrow, typedDigest} from './base-authorization.mjs';

const key = new Uint8Array(32);
key[31] = 7;
const wallet = '0x' + Buffer.from(keccak_256(secp256k1.getPublicKey(key, false).subarray(1)).subarray(12))
  .toString('hex');
const NOW = 1_790_000_000;
const USDC_DOMAIN = {name: 'USD Coin', version: '2', chainId: '8453',
  verifyingContract: '0x833589fcd6edb6e08f4c7c32d4f71b54bda02913'};
const COW_DOMAIN = {name: 'Gnosis Protocol', version: 'v2', chainId: 8453,
  verifyingContract: '0x9008d19f58aabd9ed0d60971565aa8510560ab41'};
const field = (name, type) => ({name, type});

// What Relay returns for a gasless Base → Solana move.
function deposit(message = {}) {
  return {
    types: {ReceiveWithAuthorization: [field('from', 'address'), field('to', 'address'),
      field('value', 'uint256'), field('validAfter', 'uint256'), field('validBefore', 'uint256'),
      field('nonce', 'bytes32')]},
    primaryType: 'ReceiveWithAuthorization',
    domain: USDC_DOMAIN,
    message: {from: wallet, to: '0xccc88a9d1b4ed6b0eaba998850414b24f1c315be', value: '5000000',
      validAfter: '0', validBefore: '1790807789',
      nonce: '0xc5d45ec13b4d970bdd7cc5de84e783c807df2d1c645831e6d8b12717e7bdc87b', ...message},
  };
}
// What the engine builds for a CoW gas top-up.
function permit(message = {}) {
  return {
    types: {Permit: [field('owner', 'address'), field('spender', 'address'), field('value', 'uint256'),
      field('nonce', 'uint256'), field('deadline', 'uint256')]},
    primaryType: 'Permit',
    domain: USDC_DOMAIN,
    message: {owner: wallet, spender: '0xc92e8bdf79f0507f65a392b0ab4667716bfe0110', value: '500000',
      nonce: '0', deadline: String(NOW + 3600), ...message},
  };
}
function order(message = {}) {
  return {
    types: {Order: [field('sellToken', 'address'), field('buyToken', 'address'), field('receiver', 'address'),
      field('sellAmount', 'uint256'), field('buyAmount', 'uint256'), field('validTo', 'uint32'),
      field('appData', 'bytes32'), field('feeAmount', 'uint256'), field('kind', 'string'),
      field('partiallyFillable', 'bool'), field('sellTokenBalance', 'string'), field('buyTokenBalance', 'string')]},
    primaryType: 'Order',
    domain: COW_DOMAIN,
    message: {sellToken: '0x833589fcd6edb6e08f4c7c32d4f71b54bda02913',
      buyToken: '0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee', receiver: wallet, sellAmount: '500000',
      buyAmount: '181136261922604', validTo: NOW + 1200, appData: '0x' + 'ab'.repeat(32), feeAmount: '0',
      kind: 'sell', partiallyFillable: false, sellTokenBalance: 'erc20', buyTokenBalance: 'erc20', ...message},
  };
}
const sign = (typed, with_ = key) => {
  const s = secp256k1.sign(typedDigest(typed), with_);
  return '0x' + s.toCompactHex() + (27 + s.recovery).toString(16);
};

test('domains match USDC and CoW settlement on Base', () => {
  // DOMAIN_SEPARATOR() of USDC and domainSeparator() of GPv2Settlement on Base mainnet.
  assert.equal(Buffer.from(domainSeparator({...USDC_DOMAIN, chainId: 8453})).toString('hex'),
    '02fa7265e7c5d81118673727957699e4d68f74cd74b7db77da710fe8a2c7834f');
  assert.equal(Buffer.from(domainSeparator(COW_DOMAIN)).toString('hex'),
    'd72ffa789b6fae41254d0b5a13e6e1e92ed947ec6a251edf1cf0b6c02c257b4b');
});

test('each shape is rebuilt in Privy format and its signature checked', () => {
  for (const typed of [deposit(), permit(), order()]) {
    const rebuilt = signable(typed, wallet.toUpperCase().replace('0X', '0x'), NOW);
    assert.equal(rebuilt.primary_type, typed.primaryType);
    assert.deepEqual(Object.keys(rebuilt.types), [typed.primaryType]);
    assert.equal(rebuilt.domain.chainId, 8453);
    checkSignature(rebuilt, sign(rebuilt), wallet);
    const other = new Uint8Array(32);
    other[31] = 8;
    assert.throws(() => checkSignature(rebuilt, sign(rebuilt, other), wallet), /another wallet/);
  }
});

test('deposits go only to pinned receivers, from this wallet', () => {
  const refuse = (typed, pattern) => assert.throws(() => signable(typed, wallet, NOW), pattern);
  refuse({...deposit(), domain: {...USDC_DOMAIN, verifyingContract: '0x0000000000000000000000000000000000000001'}},
    /not something/);
  refuse({...deposit(), domain: {...USDC_DOMAIN, chainId: '1'}}, /not something/);
  refuse(deposit({from: '0x0000000000000000000000000000000000000002'}), /this wallet/);
  refuse(deposit({to: '0x0000000000000000000000000000000000000003'}), /receiver/);
  // Layerswap's gasless receiver is no longer pinned.
  refuse(deposit({to: '0x6351c235e6f7e08f80974009d01829e5a8250d62'}), /receiver/);
  refuse(deposit({value: '0'}), /invalid/);
  refuse(deposit({nonce: '0x12'}), /invalid/);
  refuse({...deposit(), primaryType: 'TransferWithAuthorization'}, /not something/);
});

test('CoW gets at most a gas top-up, sold for ETH to this wallet, briefly', () => {
  const refuse = (typed, pattern) => assert.throws(() => signable(typed, wallet, NOW), pattern);
  refuse(permit({spender: '0x0000000000000000000000000000000000000004'}), /spender/);
  refuse(permit({value: '2000001'}), /top-up/);
  refuse(permit({deadline: String(NOW + 3 * 3600)}), /expires/);
  refuse(permit({deadline: String(NOW - 1)}), /expires/);
  refuse(permit({owner: '0x0000000000000000000000000000000000000005'}), /this wallet/);
  refuse(order({receiver: '0x0000000000000000000000000000000000000006'}), /this wallet/);
  refuse(order({buyToken: '0x4200000000000000000000000000000000000006'}), /USDC to ETH/);
  refuse(order({sellAmount: '5000000'}), /top-up/);
  refuse(order({partiallyFillable: true}), /plain sell/);
  refuse(order({feeAmount: '1'}), /plain sell/);
  refuse(order({kind: 'buy'}), /plain sell/);
  refuse(order({validTo: NOW + 3 * 3600}), /expires/);
  refuse({...order(), domain: {...COW_DOMAIN, verifyingContract: '0x0000000000000000000000000000000000000007'}},
    /not something/);
  refuse({...order(), primaryType: 'toString'}, /not something/);
});

test('a link secret signs only a payout from its own escrow to a pinned receiver', () => {
  const secret = '0x' + '11'.repeat(32);
  const {address} = escrowKey(secret);
  const payout = deposit({from: address, to: '0xccc88a9d1b4ed6b0eaba998850414b24f1c315be'});
  const signed = signForEscrow(payout, secret, NOW);
  assert.equal(signed.address, address);
  checkSignature(signable(payout, address, NOW), signed.signature, address);
  // Not from the escrow, not to a pinned receiver, or not a payout at all: refused.
  assert.throws(() => signForEscrow(deposit(), secret, NOW), /this wallet/);
  assert.throws(() => signForEscrow(deposit({from: address, to: '0x0000000000000000000000000000000000000009'}),
    secret, NOW), /receiver/);
  assert.throws(() => signForEscrow(permit({owner: address}), secret, NOW), /pinned receiver/);
  assert.throws(() => escrowKey('0x12'), /secret/);
  assert.throws(() => escrowKey('0x' + '00'.repeat(32)), /secret/);
});
