import assert from 'node:assert/strict';
import {generateKeyPairSync} from 'node:crypto';
import {test} from 'node:test';
import {deriveTradeSubkey, signSubkeyAuth, signParadexOrder} from './trade-subkey.mjs';
import {subkeyRegistrationMessage} from './paradex-onboarding.mjs';

const {privateKey} = generateKeyPairSync('ec', {namedCurve: 'prime256v1'});
const seed = privateKey.export({format: 'der', type: 'pkcs8'}).toString('base64');
const wallet = '0xEe8646AF9e1DDA672716389aB64a7bD0Fd202ba7';
const account = '0x3ab795501073e1fc9fb78321a57626e3569111c7eb766db9ccec1c54db227c';
const chain = 'PRIVATE_SN_PARACLEAR_TESTNET';

test('trade subkeys are stable and separated by owner, wallet, and environment', () => {
  const first = deriveTradeSubkey(seed, 'testnet', 'did:privy:user1', wallet);
  assert.deepEqual(first, deriveTradeSubkey(seed, 'testnet', 'did:privy:user1', wallet));
  assert.notEqual(first.publicKey, deriveTradeSubkey(seed, 'prod', 'did:privy:user1', wallet).publicKey);
  assert.notEqual(first.publicKey, deriveTradeSubkey(seed, 'testnet', 'did:privy:user2', wallet).publicKey);
});
test('registration statement binds the exact subkey', () => {
  const key = deriveTradeSubkey(seed, 'testnet', 'did:privy:user1', wallet);
  const message = subkeyRegistrationMessage('testnet', wallet, key.publicKey, '1234567890abcdef');
  assert.ok(message.includes('Paradex Subkey Registration: ' + key.publicKey));
  assert.ok(message.includes('Chain ID: 11155111'));
});
test('subkey signs Paradex authentication and an exact order', () => {
  const key = deriveTradeSubkey(seed, 'testnet', 'did:privy:user1', wallet);
  const auth = signSubkeyAuth(key, account, chain, 1780000000);
  assert.equal(JSON.parse(auth.signature).length, 2);
  assert.equal(auth.expiration, 1780000300);
  const order = {market: 'BTC-USD-PERP', side: 'BUY', type: 'MARKET', size: '0.001', price: '0'};
  assert.equal(JSON.parse(signParadexOrder(key, account, chain, order, 1780000000000)).length, 2);
  assert.throws(() => signParadexOrder(key, account, chain, {...order, size: '0.000000001'}, 1780000000000));
});
