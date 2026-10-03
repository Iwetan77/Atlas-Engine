import assert from 'node:assert/strict';
import {test} from 'node:test';
import {secp256k1} from '@noble/curves/secp256k1';
import {keccak_256} from '@noble/hashes/sha3';
import {actionHash, agentDigest, agentFor, approveDigest, cancelAction, checkApproval, leverageAction, moveAction,
  orderAction, signAsAgent, tpslAction, wire} from './hyperliquid.mjs';

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

test('markets with margin per position only get isolated leverage', () => {
  assert.deepEqual(leverageAction({asset: 110004, leverage: 5, isolated: true}),
    {type: 'updateLeverage', asset: 110004, isCross: false, leverage: 5});
  assert.throws(() => leverageAction({asset: 1, leverage: 5, isolated: 'yes'}), /invalid/);
});

test("margin moves only between the user's own dexes, in USDC, to their own wallet", () => {
  const wallet = '0xAbC0000000000000000000000000000000000001';
  // The shape the live exchange took on 2026-10-01 (it recovered the signing agent).
  const action = moveAction({wallet, from: '', to: 'xyz', amount: '1500000'}, 1790000000000);
  assert.deepEqual(Object.keys(action),
    ['type', 'destination', 'sourceDex', 'destinationDex', 'token', 'amount', 'fromSubAccount', 'nonce']);
  assert.equal(action.type, 'agentSendAsset');
  assert.equal(action.destination, wallet.toLowerCase());
  assert.equal(action.token, 'USDC:0x6d1e7cde53ba9467b783cb7c530ce054');
  assert.equal(action.amount, '1.500000');
  assert.equal(moveAction({wallet, from: 'xyz', to: '', amount: '42'}, 1).amount, '0.000042');
  assert.throws(() => moveAction({wallet, from: '', to: 'flx', amount: '1'}, 1), /invalid/);
  assert.throws(() => moveAction({wallet, from: 'xyz', to: 'xyz', amount: '1'}, 1), /invalid/);
  assert.throws(() => moveAction({wallet, from: '', to: 'xyz', amount: '1.5'}, 1), /invalid/);
  assert.throws(() => moveAction({wallet, from: '', to: 'xyz', amount: '0'}, 1), /invalid/);
  assert.throws(() => moveAction({wallet: 'nobody', from: '', to: 'xyz', amount: '1'}, 1), /invalid/);
});

test("a position's take-profit and stop-loss are market triggers on the whole position", () => {
  const action = tpslAction({asset: 0, isBuy: false, orders: [
    {tpsl: 'tp', trigger: '120000.0', price: '108000'},
    {tpsl: 'sl', trigger: '90000', price: '81000'},
  ]});
  assert.deepEqual(action, {type: 'order', orders: [
    {a: 0, b: false, p: '108000', s: '0', r: true, t: {trigger: {isMarket: true, triggerPx: '120000', tpsl: 'tp'}}},
    {a: 0, b: false, p: '81000', s: '0', r: true, t: {trigger: {isMarket: true, triggerPx: '90000', tpsl: 'sl'}}},
  ], grouping: 'positionTpsl'});
  // Field order matters: it's what gets hashed (Hyperliquid's SDK order).
  assert.deepEqual(Object.keys(action.orders[0].t.trigger), ['isMarket', 'triggerPx', 'tpsl']);
  assert.throws(() => tpslAction({asset: 0, isBuy: false, orders: []}), /invalid/);
  assert.throws(() => tpslAction({asset: 0, isBuy: false, orders: [{tpsl: 'tp', trigger: '1', price: '1'},
    {tpsl: 'tp', trigger: '2', price: '2'}]}), /invalid/);
  assert.throws(() => tpslAction({asset: 0, isBuy: false, orders: [{tpsl: 'limit', trigger: '1', price: '1'}]}), /invalid/);
  assert.throws(() => tpslAction({asset: 0, isBuy: false, orders: [{tpsl: 'sl', trigger: '0', price: '1'}]}), /invalid/);
  assert.deepEqual(cancelAction({asset: 110004, oids: [564383182882]}),
    {type: 'cancel', cancels: [{a: 110004, o: 564383182882}]});
  assert.throws(() => cancelAction({asset: 1, oids: []}), /invalid/);
  assert.throws(() => cancelAction({asset: 1, oids: ['5']}), /invalid/);
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
