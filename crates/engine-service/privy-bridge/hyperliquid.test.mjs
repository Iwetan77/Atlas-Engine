import assert from 'node:assert/strict';
import {test} from 'node:test';
import {secp256k1} from '@noble/curves/secp256k1';
import {keccak_256} from '@noble/hashes/sha3';
import {actionHash, agentDigest, agentFor, approveDigest, checkApproval, leverageAction, orderAction, signAsAgent,
  wire} from './hyperliquid.mjs';

const recover = (digest, sig) => {
  const signature = new secp256k1.Signature(BigInt(sig.r), BigInt(sig.s)).addRecoveryBit(sig.v - 27);
  return '0x' + Buffer.from(keccak_256(signature.recoverPublicKey(digest).toRawBytes(false).subarray(1)).subarray(12))
    .toString('hex');
};

test('the agent is stable per user and wallet, and differs between them', () => {
  const a = agentFor('secret', 'did:privy:a', '0xAbC0000000000000000000000000000000000001');
  assert.equal(a.address, agentFor('secret', 'did:privy:a', '0xabc0000000000000000000000000000000000001').address);
  assert.notEqual(a.address, agentFor('secret', 'did:privy:b', '0xabc0000000000000000000000000000000000001').address);
  assert.notEqual(a.address, agentFor('other', 'did:privy:a', '0xabc0000000000000000000000000000000000001').address);
  assert.throws(() => agentFor('', 'did:privy:a', '0x1'), /secret/);
});

test('an order hashes exactly as Hyperliquid checks it, and its signature recovers the agent', () => {
  // The same encoding the live exchange accepted on 2026-10-01 (it recovered the signing agent).
  const action = orderAction({asset: 74, isBuy: true, price: '5.30', size: '2', reduceOnly: false});
  assert.deepEqual(Object.keys(action), ['type', 'orders', 'grouping']);
  assert.deepEqual(Object.keys(action.orders[0]), ['a', 'b', 'p', 's', 'r', 't']);
  assert.equal(action.orders[0].p, '5.3');
  assert.equal(Buffer.from(actionHash(action, 1790000000000)).toString('hex'),
    Buffer.from(actionHash(orderAction({asset: 74, isBuy: true, price: '5.3', size: '2.0', reduceOnly: false}),
      1790000000000)).toString('hex'));
  const agent = agentFor('secret', 'did:privy:a', '0xabc0000000000000000000000000000000000001');
  const sig = signAsAgent(agent.key, action, 1790000000000);
  assert.equal(recover(agentDigest(action, 1790000000000), sig), agent.address);
  assert.deepEqual(leverageAction({asset: 0, leverage: 10}), {type: 'updateLeverage', asset: 0, isCross: true, leverage: 10});
});

test('bad orders, leverage and numbers are refused', () => {
  assert.throws(() => orderAction({asset: -1, isBuy: true, price: '1', size: '1', reduceOnly: false}), /invalid/);
  assert.throws(() => orderAction({asset: 1, isBuy: true, price: '1', size: '0', reduceOnly: false}), /invalid/);
  assert.throws(() => orderAction({asset: 1, isBuy: 'yes', price: '1', size: '1', reduceOnly: false}), /invalid/);
  assert.throws(() => leverageAction({asset: 1, leverage: 0}), /invalid/);
  assert.throws(() => wire('1e5'), /invalid/);
  assert.throws(() => wire('-1'), /invalid/);
  assert.equal(wire('0.000100'), '0.0001');
  assert.equal(wire('120'), '120');
});

test('only the user approving exactly this agent passes', () => {
  const user = new Uint8Array(32); user[31] = 9;
  const wallet = '0x' + Buffer.from(keccak_256(secp256k1.getPublicKey(user, false).subarray(1)).subarray(12)).toString('hex');
  const agent = '0x1cb0eb176b8993e9637818a5b0cd6827468bff44';
  const signed = secp256k1.sign(approveDigest(agent, 42), user);
  const signature = '0x' + signed.toCompactHex() + (27 + signed.recovery).toString(16);
  assert.equal(checkApproval(agent, 42, signature, wallet).v, 27 + signed.recovery);
  assert.throws(() => checkApproval('0x0000000000000000000000000000000000000001', 42, signature, wallet), /another wallet/);
  assert.throws(() => checkApproval(agent, 43, signature, wallet), /another wallet/);
});
