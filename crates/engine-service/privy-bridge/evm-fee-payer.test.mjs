import test from 'node:test';
import assert from 'node:assert/strict';
import {checkTopUp, recordTopUp} from './evm-fee-payer.mjs';

const owner = '0x16575aada6b1140f6c263f426764ffea08e3eda9';

test('the fee payer sends gas only to the signed-in wallet, capped, on a known chain', () => {
  const ok = checkTopUp({chain: 'robinhood', to: owner, amount: '26000000000000', owner, userId: 'u-1'});
  assert.equal(ok.value, 26_000_000_000_000n);
  assert.equal(ok.config.id, 4663);
  assert.throws(() => checkTopUp({chain: 'robinhood', to: '0x1111111111111111111111111111111111111111', amount: '1', owner, userId: 'u-1'}), /own wallet/);
  assert.throws(() => checkTopUp({chain: 'robinhood', to: owner, amount: '100000000000001', owner, userId: 'u-1'}), /too large/);
  assert.throws(() => checkTopUp({chain: 'base', to: owner, amount: '1', owner, userId: 'u-1'}), /unsupported/);
  assert.throws(() => checkTopUp({chain: 'robinhood', to: owner, amount: '0', owner, userId: 'u-1'}), /invalid/);
});

test('one user gets at most six top-ups an hour', () => {
  const now = Date.now();
  for (let i = 0; i < 6; i++) recordTopUp('u-2', undefined, now - i * 1000);
  assert.throws(() => checkTopUp({chain: 'robinhood', to: owner, amount: '1', owner, userId: 'u-2', now}), /too many/);
  // An hour later the window has passed.
  assert.ok(checkTopUp({chain: 'robinhood', to: owner, amount: '1', owner, userId: 'u-2', now: now + 3_601_000}));
});
