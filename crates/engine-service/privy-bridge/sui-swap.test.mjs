import assert from 'node:assert/strict';
import {test} from 'node:test';
import {Ed25519Keypair} from '@mysten/sui/keypairs/ed25519';
import {blake2b} from '@noble/hashes/blake2b';
import {intentMessage, serializedSignature, walletPublicKey} from './sui-swap.mjs';

test('a raw ed25519 signature over blake2b(intent || tx) becomes a Sui signature that verifies', async () => {
  const keypair = new Ed25519Keypair();
  const txBytes = new Uint8Array([1, 2, 3, 4, 5]);
  // What Privy rawSign does with hash_function blake2b256: hash, then sign the 32-byte digest.
  const digest = blake2b(intentMessage(txBytes), {dkLen: 32});
  const raw = await keypair.sign(digest);
  const publicKey = keypair.getPublicKey();
  const signature = serializedSignature(`0x${Buffer.from(raw).toString('hex')}`, publicKey);
  assert.equal(await publicKey.verifyTransaction(txBytes, signature), true);
});

test('the wallet key is accepted in hex, base64 or with a scheme flag, only if it matches the address', () => {
  const keypair = new Ed25519Keypair();
  const key = keypair.getPublicKey();
  const address = key.toSuiAddress();
  const bytes = key.toRawBytes();
  const hex = `0x${Buffer.from(bytes).toString('hex')}`;
  assert.equal(walletPublicKey(hex, address).toSuiAddress(), address);
  assert.equal(walletPublicKey(Buffer.from(bytes).toString('base64'), address).toSuiAddress(), address);
  assert.equal(walletPublicKey(`0x00${hex.slice(2)}`, address).toSuiAddress(), address);
  assert.throws(() => walletPublicKey(hex, new Ed25519Keypair().getPublicKey().toSuiAddress()));
});
