import assert from 'node:assert/strict';
import {test} from 'node:test';
import {Ed25519Keypair} from '@mysten/sui/keypairs/ed25519';
import {blake2b} from '@noble/hashes/blake2b';
import {intentMessage, serializedSignature, walletPublicKey, prepareSuiCashout, saleEffects, executionResult, checkRecoveryQuote} from './sui-swap.mjs';

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
test('sale effects use only the signed wallet net SUI credit, never the estimated quote',()=>{
  const result={digest:'tx',effects:{status:{status:'success'}},balanceChanges:[
    {coinType:'0x2::sui::SUI',owner:{AddressOwner:'mine'},amount:'999000'},
    {coinType:'0x2::sui::SUI',owner:{AddressOwner:'other'},amount:'1000000'},
    {coinType:'coin',owner:{AddressOwner:'mine'},amount:'-100'}]};
  assert.equal(saleEffects(result,'mine').amountOut,'999000');
  result.effects.status.status='failure';assert.equal(saleEffects(result,'mine').ok,false);
});

test('gRPC execution counts canonical SUI credits and preserves failed outcomes', () => {
  const tx = {digest:'confirmed',status:{success:true,error:null},balanceChanges:[
    {coinType:'0x'+'0'.repeat(63)+'2::sui::SUI',address:'mine',amount:'1257151707'},
    {coinType:'0x2::sui::SUI',address:'other',amount:'9000000000'}
  ]};
  const result = executionResult({Transaction:tx});
  assert.equal(saleEffects(result,'mine').amountOut,'1257151707');
  assert.equal(saleEffects(result,'mine').ok,true);
  tx.status={success:false,error:{message:'out of gas'}};
  const failed=executionResult({FailedTransaction:tx});
  assert.equal(saleEffects(failed,'mine').ok,false);
  assert.equal(failed.effects.status.error,'out of gas');
  assert.throws(()=>executionResult({}),/outcome unavailable/);
  assert.throws(()=>executionResult({Transaction:{digest:'unknown'}}),/outcome unavailable/);
});

test('recovery requires a live quote and enforces minimum after swap slippage',()=>{
 const good={minimumOut:'970',output:'1000',expiresAtUnixMs:Date.now()+30000};
 assert.doesNotThrow(()=>checkRecoveryQuote(good));
 assert.doesNotThrow(()=>checkRecoveryQuote({...good,output:'980'}));
 assert.throws(()=>checkRecoveryQuote({...good,output:'970'}),/nothing was signed/);
 assert.throws(()=>checkRecoveryQuote({...good,output:'969'}),/nothing was signed/);
 assert.throws(()=>checkRecoveryQuote({...good,expiresAtUnixMs:Date.now()-1}));
 assert.throws(()=>checkRecoveryQuote({...good,minimumOut:undefined}));
 assert.throws(()=>checkRecoveryQuote({...good,minimumOut:'0'}));
});

test("the request the device approves is byte for byte the one Privy's SDK sends for raw_sign", async () => {
  const {createRequire} = await import('node:module');
  const require = createRequire(import.meta.url);
  const {generateKeyPairSync} = await import('node:crypto');
  const auth = require(new URL('./node_modules/@privy-io/node/lib/authorization.js', import.meta.url).pathname);
  const {rawSignRequest} = await import('./sui-swap.mjs');
  const key = generateKeyPairSync('ec', {namedCurve: 'P-256'}).privateKey
    .export({format: 'der', type: 'pkcs8'}).toString('base64');
  const requestExpiry = Date.now() + 180000;
  const {params, request} = rawSignRequest({appId: 'app-123', walletId: 'w-1', message: '0xabcd', requestExpiry});
  // What the SDK signs when the bridge calls privy.wallets().rawSign(...) with these params.
  const {headers} = await auth.prepareRequest(null, 'app-123', {
    authorizationContext: {authorization_private_keys: [key]}, requestExpiry,
    method: 'POST', url: 'https://api.privy.io/v1/wallets/w-1/raw_sign', body: {params},
  });
  // What the device signs: the request the engine hands it.
  const device = auth.generateAuthorizationSignature({authorizationPrivateKey: key,
    input: auth.formatRequestForAuthorizationSignature(structuredClone(request))});
  assert.equal(device, headers['privy-authorization-signature']);
  assert.equal(headers['privy-request-expiry'], String(requestExpiry));
});


test('a sale needs its actual unsigned gas budget; the later cashout still reserves 0.02 SUI',async()=>{
  const {checkSaleGas}=await import('./sui-swap.mjs');
  assert.doesNotThrow(()=>checkSaleGas('1000000',18221624n));
  assert.throws(()=>checkSaleGas('20000000',18221624n));
  assert.throws(()=>checkSaleGas(undefined,18221624n));
  assert.throws(()=>checkSaleGas('0',18221624n));
});
