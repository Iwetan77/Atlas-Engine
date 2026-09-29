import {createPrivateKey, createPublicKey} from 'node:crypto';

export function parseSignerConfig(environment) {
  const signerId = environment.PRIVY_SERVER_SIGNER_ID?.trim();
  const privateKey = environment.PRIVY_SERVER_AUTH_PRIVATE_KEY?.trim();
  if (!signerId || !privateKey) return null;
  const policyIds = (environment.PRIVY_SERVER_SIGNER_POLICY_IDS ?? '')
    .split(',').map((value) => value.trim()).filter(Boolean);
  if (policyIds.length > 1) throw new Error('Privy allows at most one signer override policy');
  const key = createPrivateKey({
    key: Buffer.from(privateKey, 'base64'),
    format: 'der',
    type: 'pkcs8',
  });
  if (key.asymmetricKeyType !== 'ec' || key.asymmetricKeyDetails?.namedCurve !== 'prime256v1') {
    throw new Error('server signer must be a P-256 key');
  }
  const publicKey = createPublicKey(key).export({format: 'der', type: 'spki'}).toString('base64');
  return {signerId, privateKey, publicKey, policyIds};
}

export function walletHasSigner(wallet, expectedAddress, signerId) {
  return wallet?.chain_type === 'ethereum' &&
    wallet.address?.toLowerCase() === expectedAddress.toLowerCase() &&
    Array.isArray(wallet.additional_signers) &&
    wallet.additional_signers.some((signer) => signer.signer_id === signerId);
}
