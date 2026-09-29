import assert from 'node:assert/strict';
import {generateKeyPairSync} from 'node:crypto';
import {test} from 'node:test';
import {parseSignerConfig, walletHasSigner} from './signer-config.mjs';

const {privateKey} = generateKeyPairSync('ec', {namedCurve: 'prime256v1'});
const encoded = privateKey.export({format: 'der', type: 'pkcs8'}).toString('base64');

test('signer config stays unavailable without both required values', () => {
  assert.equal(parseSignerConfig({}), null);
  assert.equal(parseSignerConfig({PRIVY_SERVER_SIGNER_ID: 'quorum'}), null);
});
test('signer config exposes only requested policy IDs', () => {
  const config = parseSignerConfig({
    PRIVY_SERVER_SIGNER_ID: 'quorum',
    PRIVY_SERVER_AUTH_PRIVATE_KEY: encoded,
  });
  assert.equal(config.signerId, 'quorum');
  assert.deepEqual(config.policyIds, []);
  assert.ok(config.publicKey);
  assert.deepEqual(parseSignerConfig({
    PRIVY_SERVER_SIGNER_ID: 'quorum',
    PRIVY_SERVER_AUTH_PRIVATE_KEY: encoded,
    PRIVY_SERVER_SIGNER_POLICY_IDS: 'policy-one',
  }).policyIds, ['policy-one']);
});
test('wallet signer status requires the right EVM wallet and quorum', () => {
  const wallet = {
    chain_type: 'ethereum',
    address: '0xAa',
    additional_signers: [{signer_id: 'quorum'}],
  };
  assert.equal(walletHasSigner(wallet, '0xaa', 'quorum'), true);
  assert.equal(walletHasSigner(wallet, '0xbb', 'quorum'), false);
  assert.equal(walletHasSigner(wallet, '0xaa', 'other'), false);
  assert.equal(walletHasSigner({...wallet, additional_signers: []}, '0xaa', 'quorum'), false);
});
