// Hyperliquid margin back to cash: Relay pays it out as USDC to the user's own Solana (or Base)
// wallet. The bridge asks Relay for the quote itself, with the recipient taken from the verified
// Privy user, so nothing the engine passes can send the money anywhere else. The user's wallet signs
// two things, each rebuilt from checked fields: Relay's nonce mapping (it ties the deposit to this
// order) and a Hyperliquid sendAsset of exactly the asked USDC to Relay's Hyperliquid account.
import {secp256k1} from '@noble/curves/secp256k1';
import {keccak_256} from '@noble/hashes/sha3';

export const RELAY_API = 'https://api.relay.link';
// Relay's account on Hyperliquid: the only place a cash-out's sendAsset may go.
export const RELAY_ACCOUNT = '0x66cf0aace1b4e562593bec10ec7868fba9932224';
// Hyperliquid on Relay (chain 1337, its USDC with 8 decimals), and Hyperliquid's own name for USDC.
const HL_CHAIN = 1337;
const HL_USDC = '0x00000000000000000000000000000000';
const USDC_TOKEN = 'USDC:0x6d1e7cde53ba9467b783cb7c530ce054';
const SIGNATURE_CHAIN_ID = '0x66eee';
const ZERO = '0x0000000000000000000000000000000000000000';
export const DESTINATIONS = {
  solana: {chainId: 792703809, currency: 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'},
  base: {chainId: 8453, currency: '0x833589fcd6edb6e08f4c7c32d4f71b54bda02913'},
};
// From 10 cents to a million dollars (USDC units, 6 decimals); Relay's nonce stays within 10 minutes of now.
const MIN_UNITS = 100_000n;
const MAX_UNITS = 1_000_000_000_000n;
const NONCE_WINDOW_MS = 10 * 60 * 1000;

const MAPPING_DOMAIN = {name: 'RelayNonceMapping', version: '2', chainId: 1, verifyingContract: ZERO};
const SEND_DOMAIN = {name: 'HyperliquidSignTransaction', version: '1', chainId: Number(SIGNATURE_CHAIN_ID),
  verifyingContract: ZERO};
const MAPPING = 'NonceMapping';
const SEND = 'HyperliquidTransaction:SendAsset';
const TYPES = {
  [MAPPING]: [
    {name: 'chainId', type: 'string'},
    {name: 'wallet', type: 'address'},
    {name: 'depositor', type: 'address'},
    {name: 'id', type: 'bytes32'},
    {name: 'nonce', type: 'uint256'},
  ],
  [SEND]: [
    {name: 'hyperliquidChain', type: 'string'},
    {name: 'destination', type: 'string'},
    {name: 'sourceDex', type: 'string'},
    {name: 'destinationDex', type: 'string'},
    {name: 'token', type: 'string'},
    {name: 'amount', type: 'string'},
    {name: 'fromSubAccount', type: 'string'},
    {name: 'nonce', type: 'uint64'},
  ],
};

const ADDRESS = /^0x[0-9a-fA-F]{40}$/;
const BYTES32 = /^0x[0-9a-fA-F]{64}$/;
const lower = (v) => String(v ?? '').toLowerCase();
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);

// USDC units (6 decimals) as Hyperliquid writes an amount: "5.000000".
export function decimal(units) {
  const text = units.toString().padStart(7, '0');
  return `${text.slice(0, -6)}.${text.slice(-6)}`;
}

function units(value) {
  if (!/^[1-9][0-9]{0,30}$/.test(String(value ?? ''))) throw new Error('invalid amount');
  const amount = BigInt(value);
  if (amount < MIN_UNITS || amount > MAX_UNITS) throw new Error('amount out of range');
  return amount;
}

// What the bridge asks Relay for: exactly `amount` USDC out of the user's Hyperliquid account,
// paid to their own wallet on `to`.
export function cashoutRequest({wallet, recipient, to, amount}) {
  const dest = DESTINATIONS[to];
  if (!dest || !ADDRESS.test(wallet ?? '') || !recipient) throw new Error('invalid cash-out');
  return {
    user: wallet, recipient, refundTo: wallet,
    originChainId: HL_CHAIN, destinationChainId: dest.chainId,
    originCurrency: HL_USDC, destinationCurrency: dest.currency,
    amount: (units(amount) * 100n).toString(), tradeType: 'EXACT_INPUT',
    // Dollars to dollars: 0.5% is room enough.
    slippageTolerance: '50',
  };
}

// Checks Relay's quote against the request and rebuilds what gets signed and posted. Throws unless
// it's two steps (the nonce mapping, then a sendAsset of exactly `amount` to Relay's account),
// paying USDC to `recipient` on `to`, for a fee of cents.
export function checkCashout(quote, {wallet, recipient, to, amount}, now = Date.now()) {
  const dest = DESTINATIONS[to];
  if (!dest || !ADDRESS.test(wallet ?? '')) throw new Error('invalid cash-out');
  const asked = units(amount);
  const user = lower(wallet);
  const [authorize, deposit, ...rest] = Array.isArray(quote?.steps) ? quote.steps : [];
  if (!authorize || !deposit || rest.length || authorize.items?.length !== 1 || deposit.items?.length !== 1) {
    throw new Error('expected a nonce mapping and a deposit');
  }
  const requestId = quote.requestId;
  if (!BYTES32.test(requestId ?? '') || authorize.requestId !== requestId || deposit.requestId !== requestId) {
    throw new Error('request id');
  }

  const sign = authorize.items[0].data?.sign;
  const post = authorize.items[0].data?.post;
  const mapping = sign?.value ?? {};
  if (authorize.kind !== 'signature' || sign?.signatureKind !== 'eip712' || sign.primaryType !== MAPPING ||
      !same(sign.types?.[MAPPING], TYPES[MAPPING]) || sign.domain?.name !== MAPPING_DOMAIN.name ||
      sign.domain?.version !== MAPPING_DOMAIN.version || String(sign.domain?.chainId) !== '1' ||
      lower(sign.domain?.verifyingContract) !== ZERO) {
    throw new Error('not a Relay nonce mapping');
  }
  const nonce = Number(mapping.nonce);
  if (mapping.chainId !== 'hyperliquid' || lower(mapping.wallet) !== user || lower(mapping.depositor) !== user ||
      !BYTES32.test(mapping.id ?? '') || !Number.isSafeInteger(nonce) || Math.abs(nonce - now) > NONCE_WINDOW_MS) {
    throw new Error('nonce mapping differs from the request');
  }
  const body = post?.body ?? {};
  if (post?.endpoint !== '/authorize' || post.method !== 'POST' || body.type !== 'nonce-mapping' ||
      Number(body.walletChainId) !== HL_CHAIN || lower(body.wallet) !== user || lower(body.depositor) !== user ||
      Number(body.nonce) !== nonce || lower(body.id) !== lower(mapping.id) || Number(body.signatureChainId) !== 1) {
    throw new Error('nonce mapping post differs from what is signed');
  }

  const item = deposit.items[0].data ?? {};
  const action = item.action ?? {};
  const p = action.parameters ?? {};
  if (deposit.kind !== 'transaction' || action.type !== 'sendAsset' || item.eip712PrimaryType !== SEND ||
      !same(item.eip712Types?.[SEND], TYPES[SEND])) {
    throw new Error('not a Hyperliquid sendAsset');
  }
  if (p.hyperliquidChain !== 'Mainnet' || lower(p.destination) !== RELAY_ACCOUNT || p.sourceDex !== '' ||
      p.destinationDex !== '' || p.token !== USDC_TOKEN || p.amount !== decimal(asked) || p.fromSubAccount !== '' ||
      Number(p.nonce) !== nonce || Number(item.nonce) !== nonce) {
    throw new Error('sendAsset differs from the request');
  }

  const details = quote.details ?? {};
  const moneyIn = details.currencyIn ?? {};
  const moneyOut = details.currencyOut ?? {};
  const landsHere = to === 'solana' ? details.recipient === recipient : lower(details.recipient) === lower(recipient);
  if (!landsHere || Number(moneyIn.currency?.chainId) !== HL_CHAIN || lower(moneyIn.currency?.address) !== HL_USDC ||
      String(moneyIn.amount) !== (asked * 100n).toString() || Number(moneyOut.currency?.chainId) !== dest.chainId ||
      lower(moneyOut.currency?.address) !== lower(dest.currency)) {
    throw new Error('not USDC from Hyperliquid to this wallet');
  }
  const leastOut = BigInt(/^[0-9]{1,30}$/.test(String(moneyOut.minimumAmount)) ? moneyOut.minimumAmount : '0');
  // A move costs cents; anything past 2% + $0.50 is a quote to refuse, not to sign.
  if (leastOut === 0n || leastOut + asked / 50n + 500_000n < asked) throw new Error('fee too high');

  const id = lower(mapping.id);
  return {
    requestId,
    nonce,
    amountOut: leastOut.toString(),
    mapping: {domain: MAPPING_DOMAIN, types: {[MAPPING]: TYPES[MAPPING]}, primary_type: MAPPING,
      message: {chainId: 'hyperliquid', wallet: user, depositor: user, id, nonce}},
    authorizeBody: {type: 'nonce-mapping', walletChainId: HL_CHAIN, wallet: user, nonce, id, depositor: user,
      signatureChainId: 1},
    send: {domain: SEND_DOMAIN, types: {[SEND]: TYPES[SEND]}, primary_type: SEND,
      message: {hyperliquidChain: 'Mainnet', destination: RELAY_ACCOUNT, sourceDex: '', destinationDex: '',
        token: USDC_TOKEN, amount: decimal(asked), fromSubAccount: '', nonce}},
    action: {type: 'sendAsset', signatureChainId: SIGNATURE_CHAIN_ID, hyperliquidChain: 'Mainnet',
      destination: RELAY_ACCOUNT, sourceDex: '', destinationDex: '', token: USDC_TOKEN, amount: decimal(asked),
      fromSubAccount: '', nonce},
  };
}

const word = (value) => BigInt(value).toString(16).padStart(64, '0');
const utf8Hash = (text) => Buffer.from(keccak_256(Buffer.from(text, 'utf8'))).toString('hex');

// The EIP-712 digest of one of the two shapes above (Privy's typed-data format).
export function typedDigest(typed) {
  const fields = TYPES[typed.primary_type];
  const domain = typed.domain;
  const domainHash = keccak_256(Buffer.from(
    utf8Hash('EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)') +
    utf8Hash(domain.name) + utf8Hash(domain.version) + word(domain.chainId) + word(domain.verifyingContract), 'hex'));
  const type = utf8Hash(`${typed.primary_type}(${fields.map((f) => `${f.type} ${f.name}`).join(',')})`);
  const encoded = fields.map(({name, type: kind}) => {
    const value = typed.message[name];
    if (kind === 'string') return utf8Hash(value);
    if (kind === 'bytes32') return value.slice(2);
    return word(value);
  });
  const struct = keccak_256(Buffer.from(type + encoded.join(''), 'hex'));
  return keccak_256(Buffer.concat([Buffer.from([0x19, 0x01]), domainHash, struct]));
}

// Throws unless `signature` (0x r s v) is `wallet` signing exactly `typed`; returns it split for Hyperliquid.
export function checkSigned(typed, signature, wallet) {
  if (!/^0x[0-9a-fA-F]{130}$/.test(signature ?? '')) throw new Error('invalid signature');
  const sig = Buffer.from(signature.slice(2), 'hex');
  const recovery = sig[64] >= 27 ? sig[64] - 27 : sig[64];
  if (recovery > 1) throw new Error('invalid recovery bit');
  const publicKey = secp256k1.Signature.fromCompact(sig.subarray(0, 64)).addRecoveryBit(recovery)
    .recoverPublicKey(typedDigest(typed)).toRawBytes(false);
  const signer = '0x' + Buffer.from(keccak_256(publicKey.subarray(1)).subarray(12)).toString('hex');
  if (signer !== lower(wallet)) throw new Error('signed by another wallet');
  return {r: '0x' + signature.slice(2, 66), s: '0x' + signature.slice(66, 130), v: recovery + 27};
}

async function json(response, what) {
  const text = await response.text();
  let body;
  try {
    body = JSON.parse(text);
  } catch {
    body = {message: text.slice(0, 200)};
  }
  if (!response.ok) throw new Error(`${what}: ${body?.message ?? response.status}`);
  return body;
}

// The whole cash-out: quote, check, sign the mapping and post it to Relay, sign the sendAsset and
// post it to Hyperliquid. `signTyped(typed)` is the user's own wallet signing (their session).
export async function cashOut({wallet, recipient, to, amount, signTyped, relayKey, post}) {
  const request = cashoutRequest({wallet, recipient, to, amount});
  const headers = {'content-type': 'application/json', ...(relayKey ? {'x-api-key': relayKey} : {})};
  const quote = await json(await fetch(`${RELAY_API}/quote/v2`, {method: 'POST', headers,
    body: JSON.stringify(request)}), 'Relay quote');
  const checked = checkCashout(quote, {wallet, recipient, to, amount});
  const mapped = checkSigned(checked.mapping, await signTyped(checked.mapping), wallet);
  const signature = mapped.r + mapped.s.slice(2) + mapped.v.toString(16);
  await json(await fetch(`${RELAY_API}/authorize?signature=${signature}`, {method: 'POST', headers,
    body: JSON.stringify(checked.authorizeBody)}), 'Relay authorize');
  const sent = checkSigned(checked.send, await signTyped(checked.send), wallet);
  const result = await post({action: checked.action, nonce: checked.nonce, signature: sent});
  if (result?.status !== 'ok') {
    throw new Error(`Hyperliquid refused the send: ${String(result?.response ?? 'no reason').slice(0, 160)}`);
  }
  return {requestId: checked.requestId, amountOut: checked.amountOut};
}

// Quote and pin the two typed-data steps before the phone signs either one.
export async function prepareCashOut({wallet,recipient,to,amount,relayKey}) {
  const request=cashoutRequest({wallet,recipient,to,amount});
  const headers={'content-type':'application/json',...(relayKey?{'x-api-key':relayKey}:{})};
  const quote=await json(await fetch(`${RELAY_API}/quote/v2`,{method:'POST',headers,body:JSON.stringify(request)}),'Relay quote');
  return checkCashout(quote,{wallet,recipient,to,amount});
}
export async function finishCashOut({checked,wallet,signatures,relayKey,post}) {
  if(signatures.length!==2)throw new Error('two approvals required');
  // Check both before posting anything.
  const mapped=checkSigned(checked.mapping,signatures[0],wallet),sent=checkSigned(checked.send,signatures[1],wallet);
  const signature=mapped.r+mapped.s.slice(2)+mapped.v.toString(16);
  const headers={'content-type':'application/json',...(relayKey?{'x-api-key':relayKey}:{})};
  await json(await fetch(`${RELAY_API}/authorize?signature=${signature}`,{method:'POST',headers,body:JSON.stringify(checked.authorizeBody)}),'Relay authorize');
  const result=await post({action:checked.action,nonce:checked.nonce,signature:sent});
  if(result?.status!=='ok')throw new Error('Cash transfer was refused; proceeds remain in perps');
  return {requestId:checked.requestId,amountOut:checked.amountOut};
}
