import {createHmac} from 'node:crypto';
import {ec, shortString, typedData} from 'starknet';

const ORDER = ec.starkCurve.CURVE.n;
const DOMAIN_TYPES = {
  StarkNetDomain: [
    {name: 'name', type: 'felt'},
    {name: 'chainId', type: 'felt'},
    {name: 'version', type: 'felt'},
  ],
};

function starkDomain(chainId) {
  if (!/^[A-Z0-9_]{5,64}$/.test(chainId)) throw new Error('invalid Paradex chain ID');
  return {name: 'Paradex', chainId: shortString.encodeShortString(chainId), version: '1'};
}

function signTyped(privateKey, accountAddress, data) {
  if (!/^0x[0-9a-fA-F]{1,64}$/.test(accountAddress)) {
    throw new Error('invalid Paradex account address');
  }
  const hash = typedData.getMessageHash(data, accountAddress);
  const signature = ec.starkCurve.sign(hash, privateKey);
  return JSON.stringify([signature.r.toString(), signature.s.toString()]);
}

function quantums(value) {
  if (typeof value !== 'string' || !/^(0|[1-9][0-9]*)(\.[0-9]{1,8})?$/.test(value)) {
    throw new Error('invalid order decimal');
  }
  const [whole, fraction = ''] = value.split('.');
  return (BigInt(whole) * 100_000_000n + BigInt(fraction.padEnd(8, '0') || '0')).toString();
}

export function deriveTradeSubkey(authorizationPrivateKey, environment, userId, walletAddress) {
  if (!['testnet', 'prod'].includes(environment) ||
      !/^did:privy:[A-Za-z0-9_-]+$/.test(userId) ||
      !/^0x[0-9a-fA-F]{40}$/.test(walletAddress)) {
    throw new Error('invalid trade subkey owner');
  }
  const material = Buffer.from(authorizationPrivateKey, 'base64');
  if (material.length < 32) throw new Error('invalid server key');
  const digest = createHmac('sha256', material)
    .update('Atlas Paradex trade subkey v1\0')
    .update(environment).update('\0')
    .update(userId).update('\0')
    .update(walletAddress.toLowerCase())
    .digest('hex');
  const scalar = BigInt('0x' + digest) % (ORDER - 1n) + 1n;
  const privateKey = '0x' + scalar.toString(16);
  return {privateKey, publicKey: ec.starkCurve.getStarkKey(privateKey).toLowerCase()};
}

export function signSubkeyAuth(subkey, accountAddress, chainId, timestamp) {
  if (!Number.isInteger(timestamp) || timestamp < 1) throw new Error('invalid auth timestamp');
  const expiration = timestamp + 300;
  const data = {
    domain: starkDomain(chainId),
    primaryType: 'Request',
    types: {
      ...DOMAIN_TYPES,
      Request: [
        {name: 'method', type: 'felt'},
        {name: 'path', type: 'felt'},
        {name: 'body', type: 'felt'},
        {name: 'timestamp', type: 'felt'},
        {name: 'expiration', type: 'felt'},
      ],
    },
    message: {method: 'POST', path: '/v1/auth', body: '', timestamp, expiration},
  };
  return {signature: signTyped(subkey.privateKey, accountAddress, data), timestamp, expiration};
}

export function signParadexOrder(subkey, accountAddress, chainId, order, timestamp) {
  if (!Number.isInteger(timestamp) || timestamp < 1 ||
      !/^[A-Z0-9-]{5,40}$/.test(order.market) ||
      !['BUY', 'SELL'].includes(order.side) ||
      !['MARKET', 'LIMIT'].includes(order.type)) {
    throw new Error('invalid Paradex order');
  }
  const data = {
    domain: starkDomain(chainId),
    primaryType: 'Order',
    types: {
      ...DOMAIN_TYPES,
      Order: [
        {name: 'timestamp', type: 'felt'},
        {name: 'market', type: 'felt'},
        {name: 'side', type: 'felt'},
        {name: 'orderType', type: 'felt'},
        {name: 'size', type: 'felt'},
        {name: 'price', type: 'felt'},
      ],
    },
    message: {
      timestamp,
      market: shortString.encodeShortString(order.market),
      side: order.side === 'BUY' ? '1' : '2',
      orderType: shortString.encodeShortString(order.type),
      size: quantums(order.size),
      price: quantums(order.price),
    },
  };
  return signTyped(subkey.privateKey, accountAddress, data);
}
