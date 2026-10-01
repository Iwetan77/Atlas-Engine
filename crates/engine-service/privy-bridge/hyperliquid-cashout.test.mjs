import assert from 'node:assert/strict';
import {test} from 'node:test';
import {secp256k1} from '@noble/curves/secp256k1';
import {keccak_256} from '@noble/hashes/sha3';
import {cashOut, cashoutRequest, checkCashout, checkSigned, decimal, typedDigest} from './hyperliquid-cashout.mjs';

const WALLET = '0x4838B106FCe9647Bdf1E7877BF73cE8B0BAD5f97';
const SOLANA = '9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM';
const NONCE = 1790851554283;
const ASK = {wallet: WALLET, recipient: SOLANA, to: 'solana', amount: '5000000'};
const REQUEST = '0x17908515542d6159cd24d8166de6a56ce37ee6029d95f84e89c063bbb635d494';
const ID = '0x5ab5ef6798efea85086cd6a96217c03a5f59ae4b09f1c790f17f9feb9720ec12';

// Captured from Relay's live API on 2026-10-01: 5 USDC out of a Hyperliquid account to Solana (trimmed).
function liveQuote() {
  return {
    requestId: REQUEST,
    steps: [
      {id: 'authorize', kind: 'signature', requestId: REQUEST, items: [{status: 'incomplete', data: {
        sign: {signatureKind: 'eip712',
          domain: {name: 'RelayNonceMapping', version: '2', chainId: 1,
            verifyingContract: '0x0000000000000000000000000000000000000000'},
          types: {NonceMapping: [{name: 'chainId', type: 'string'}, {name: 'wallet', type: 'address'},
            {name: 'depositor', type: 'address'}, {name: 'id', type: 'bytes32'}, {name: 'nonce', type: 'uint256'}]},
          value: {chainId: 'hyperliquid', wallet: WALLET, nonce: NONCE, id: ID, depositor: WALLET},
          primaryType: 'NonceMapping'},
        post: {endpoint: '/authorize', method: 'POST', body: {type: 'nonce-mapping', walletChainId: 1337,
          wallet: WALLET.toLowerCase(), nonce: NONCE, id: ID, depositor: WALLET, signatureChainId: 1}}}}]},
      {id: 'deposit', kind: 'transaction', requestId: REQUEST, depositAddress: '', items: [{status: 'incomplete', data: {
        action: {type: 'sendAsset', parameters: {hyperliquidChain: 'Mainnet',
          destination: '0x66cf0aace1b4e562593bec10ec7868fba9932224', sourceDex: '', destinationDex: '',
          token: 'USDC:0x6d1e7cde53ba9467b783cb7c530ce054', amount: '5.000000', fromSubAccount: '', nonce: NONCE}},
        nonce: NONCE,
        eip712Types: {'HyperliquidTransaction:SendAsset': [{name: 'hyperliquidChain', type: 'string'},
          {name: 'destination', type: 'string'}, {name: 'sourceDex', type: 'string'},
          {name: 'destinationDex', type: 'string'}, {name: 'token', type: 'string'}, {name: 'amount', type: 'string'},
          {name: 'fromSubAccount', type: 'string'}, {name: 'nonce', type: 'uint64'}]},
        eip712PrimaryType: 'HyperliquidTransaction:SendAsset'}}]},
    ],
    details: {recipient: SOLANA,
      currencyIn: {currency: {chainId: 1337, address: '0x00000000000000000000000000000000'}, amount: '500000000',
        minimumAmount: '500000000'},
      currencyOut: {currency: {chainId: 792703809, address: 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'},
        amount: '4973555', minimumAmount: '4948688'}},
  };
}

const check = (quote, ask = ASK) => checkCashout(quote, ask, NONCE + 1000);
const changed = (edit, ask) => {
  const quote = liveQuote();
  edit(quote);
  return () => check(quote, ask);
};
const sign = (key, typed) => {
  const signed = secp256k1.sign(typedDigest(typed), key);
  return '0x' + signed.toCompactHex() + (27 + signed.recovery).toString(16);
};
const addressOf = (key) => '0x' + Buffer.from(keccak_256(secp256k1.getPublicKey(key, false).subarray(1)).subarray(12))
  .toString('hex');

test('amounts are written as Hyperliquid writes them', () => {
  assert.equal(decimal(5_000_000n), '5.000000');
  assert.equal(decimal(123_456n), '0.123456');
  assert.equal(decimal(1_000_000_001n), '1000.000001');
});

test('the bridge asks Relay for exactly the amount, to the given wallet, in Hyperliquid units', () => {
  const request = cashoutRequest(ASK);
  assert.equal(request.amount, '500000000');
  assert.equal(request.recipient, SOLANA);
  assert.equal(request.originChainId, 1337);
  assert.equal(request.destinationChainId, 792703809);
  assert.equal(cashoutRequest({...ASK, to: 'base', recipient: WALLET}).destinationChainId, 8453);
  assert.throws(() => cashoutRequest({...ASK, to: 'ethereum'}));
  assert.throws(() => cashoutRequest({...ASK, amount: '99999'}), /range/);
  assert.throws(() => cashoutRequest({...ASK, amount: '1.5'}), /invalid amount/);
});

test("Relay's live quote reads as one mapping and one send of exactly the amount to Relay", () => {
  const checked = check(liveQuote());
  assert.equal(checked.requestId, REQUEST);
  assert.equal(checked.nonce, NONCE);
  assert.equal(checked.amountOut, '4948688');
  assert.deepEqual(checked.action, {type: 'sendAsset', signatureChainId: '0x66eee', hyperliquidChain: 'Mainnet',
    destination: '0x66cf0aace1b4e562593bec10ec7868fba9932224', sourceDex: '', destinationDex: '',
    token: 'USDC:0x6d1e7cde53ba9467b783cb7c530ce054', amount: '5.000000', fromSubAccount: '', nonce: NONCE});
  assert.equal(checked.mapping.message.wallet, WALLET.toLowerCase());
  assert.equal(checked.authorizeBody.id, ID);
});

test('refuses a quote that pays anyone else, sends elsewhere or more, or differs anywhere', () => {
  const send = (q) => q.steps[1].items[0].data.action.parameters;
  const mapping = (q) => q.steps[0].items[0].data.sign.value;
  const body = (q) => q.steps[0].items[0].data.post.body;
  assert.throws(changed((q) => { q.details.recipient = 'Other1111111111111111111111111111'; }), /this wallet/);
  assert.throws(changed(() => {}, {...ASK, recipient: 'Other1111111111111111111111111111'}), /this wallet/);
  assert.throws(changed((q) => { q.details.currencyOut.currency.chainId = 8453; }), /this wallet/);
  assert.throws(changed((q) => { send(q).destination = '0x0000000000000000000000000000000000000001'; }), /sendAsset/);
  assert.throws(changed((q) => { send(q).amount = '50.000000'; }), /sendAsset/);
  assert.throws(changed((q) => { send(q).token = 'PURR:0xc1fb593aeffbeb02f85e0308e9956a90'; }), /sendAsset/);
  assert.throws(changed((q) => { send(q).sourceDex = 'xyz'; }), /sendAsset/);
  assert.throws(changed((q) => { send(q).fromSubAccount = '0x0000000000000000000000000000000000000002'; }), /sendAsset/);
  assert.throws(changed((q) => { send(q).nonce = NONCE + 1; }), /sendAsset/);
  assert.throws(changed((q) => { mapping(q).wallet = '0x0000000000000000000000000000000000000003'; }), /mapping/);
  assert.throws(changed((q) => { mapping(q).depositor = '0x0000000000000000000000000000000000000003'; }), /mapping/);
  assert.throws(changed((q) => { body(q).id = '0x' + '11'.repeat(32); }), /mapping post/);
  assert.throws(changed((q) => { q.steps[0].items[0].data.sign.domain.chainId = 8453; }), /nonce mapping/);
  assert.throws(changed((q) => { q.steps[0].items[0].data.post.endpoint = '/execute/permits'; }), /mapping post/);
  assert.throws(changed((q) => { q.details.currencyIn.amount = '600000000'; }), /this wallet/);
  // Far less landing than sent.
  assert.throws(changed((q) => { q.details.currencyOut.minimumAmount = '4000000'; }), /fee/);
  assert.throws(changed((q) => { q.steps.push(q.steps[1]); }), /expected/);
  // An old or far-future nonce.
  assert.throws(() => checkCashout(liveQuote(), ASK, NONCE + 11 * 60 * 1000), /mapping/);
});

test('signatures recover the wallet that signed exactly the rebuilt message', () => {
  const key = secp256k1.utils.randomPrivateKey();
  const wallet = addressOf(key);
  const quote = liveQuote();
  for (const value of [quote.steps[0].items[0].data.sign.value]) {
    value.wallet = wallet;
    value.depositor = wallet;
  }
  Object.assign(quote.steps[0].items[0].data.post.body, {wallet, depositor: wallet});
  const checked = check(quote, {...ASK, wallet});
  const split = checkSigned(checked.send, sign(key, checked.send), wallet);
  assert.match(split.r, /^0x[0-9a-f]{64}$/);
  assert.ok([27, 28].includes(split.v));
  assert.throws(() => checkSigned(checked.mapping, sign(key, checked.send), wallet), /another wallet/);
  assert.throws(() => checkSigned(checked.send, sign(secp256k1.utils.randomPrivateKey(), checked.send), wallet),
    /another wallet/);
});

test('a cash-out posts the mapping to Relay before the send to Hyperliquid, and stops on a refusal', async () => {
  const key = secp256k1.utils.randomPrivateKey();
  const wallet = addressOf(key);
  const calls = [];
  const realFetch = globalThis.fetch;
  globalThis.fetch = async (url, init) => {
    calls.push(String(url));
    if (String(url).endsWith('/quote/v2')) {
      const quote = liveQuote();
      quote.steps[0].items[0].data.sign.value.nonce = Date.now();
      quote.steps[0].items[0].data.post.body.nonce = quote.steps[0].items[0].data.sign.value.nonce;
      const nonce = quote.steps[0].items[0].data.sign.value.nonce;
      quote.steps[1].items[0].data.action.parameters.nonce = nonce;
      quote.steps[1].items[0].data.nonce = nonce;
      Object.assign(quote.steps[0].items[0].data.sign.value, {wallet, depositor: wallet});
      Object.assign(quote.steps[0].items[0].data.post.body, {wallet, depositor: wallet});
      assert.equal(JSON.parse(init.body).recipient, SOLANA);
      return new Response(JSON.stringify(quote));
    }
    return new Response('{}');
  };
  try {
    const posted = [];
    const result = await cashOut({wallet, recipient: SOLANA, to: 'solana', amount: '5000000',
      signTyped: async (typed) => sign(key, typed),
      post: async (body) => { posted.push(body); return {status: 'ok'}; }});
    assert.equal(result.requestId, REQUEST);
    assert.equal(calls.length, 2);
    assert.match(calls[1], /\/authorize\?signature=0x[0-9a-f]{130}$/);
    assert.equal(posted[0].action.destination, '0x66cf0aace1b4e562593bec10ec7868fba9932224');
    await assert.rejects(cashOut({wallet, recipient: SOLANA, to: 'solana', amount: '5000000',
      signTyped: async (typed) => sign(key, typed),
      post: async () => ({status: 'err', response: 'Insufficient balance'})}), /Insufficient balance/);
  } finally {
    globalThis.fetch = realFetch;
  }
});
