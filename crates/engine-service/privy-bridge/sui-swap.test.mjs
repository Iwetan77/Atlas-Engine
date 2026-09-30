import assert from 'node:assert/strict';
import {test} from 'node:test';
import {Ed25519Keypair} from '@mysten/sui/keypairs/ed25519';
import {blake2b} from '@noble/hashes/blake2b';
import {intentMessage, serializedSignature, walletPublicKey, prepareSuiCashout} from './sui-swap.mjs';

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

test('cashout quote binds the same user wallets and rejects output below the approved minimum', async () => {
  const original = globalThis.fetch;
  let request;
  globalThis.fetch = async (_url, init) => {
    request = JSON.parse(init.body);
    return new Response(JSON.stringify({quote: {
      amountIn: '1000000000', amountOut: '1150000', minAmountOut: '1130000',
      depositAddress: `0x${'3'.repeat(64)}`, depositMemo: null,
    }}), {status: 200});
  };
  try {
    const wallet = {address: `0x${'1'.repeat(64)}`};
    const evmWallet = `0x${'2'.repeat(40)}`;
    const quote = await prepareSuiCashout({wallet, evmWallet, amount: '1000000000', minimumOut: '1120000'});
    assert.equal(request.recipient, evmWallet);
    assert.equal(request.refundTo, wallet.address);
    assert.equal(request.originAsset, 'nep141:sui.omft.near');
    assert.equal(quote.depositAddress, `0x${'3'.repeat(64)}`);
    await assert.rejects(() => prepareSuiCashout({
      wallet, evmWallet, amount: '1000000000', minimumOut: '1140000',
    }));
  } finally {
    globalThis.fetch = original;
  }
});