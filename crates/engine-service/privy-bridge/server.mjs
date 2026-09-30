import {createServer} from 'node:http';
import {PrivyClient} from '@privy-io/node';
import {authMessage, onboardingMessage, subkeyRegistrationMessage, recoverOnboardingPublicKey} from './paradex-onboarding.mjs';
import {deriveTradeSubkey, signSubkeyAuth, signParadexOrder} from './trade-subkey.mjs';
import {parseSignerConfig, walletHasSigner} from './signer-config.mjs';

const appId = process.env.PRIVY_APP_ID;
const appSecret = process.env.PRIVY_APP_SECRET;
if (!appId || !appSecret) throw new Error('PRIVY_APP_ID and PRIVY_APP_SECRET are required');
const privy = new PrivyClient({appId, appSecret});
const signer = parseSignerConfig(process.env);
const port = Number(process.env.PRIVY_BRIDGE_PORT ?? 3101);
if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('invalid bridge port');

function send(response, status, body) {
  response.writeHead(status, {'content-type': 'application/json'});
  response.end(JSON.stringify(body));
}

let verifiedSigner = false;
async function serverSigner() {
  if (!signer) return null;
  if (!verifiedSigner) {
    const quorum = await privy.keyQuorums().get(signer.signerId);
    if (quorum.authorization_threshold !== 1 ||
        !quorum.authorization_keys?.some((key) => key.public_key === signer.publicKey)) {
      throw new Error('Privy signer quorum does not match the authorization key');
    }
    verifiedSigner = true;
  }
  return signer;
}

async function signerStatus(evm, address) {
  const configured = await serverSigner();
  if (!configured) return {signer: null, signerAuthorized: false};
  if (typeof evm.id !== 'string' || !evm.id) {
    throw new Error('Privy embedded wallet ID unavailable');
  }
  const wallet = await privy.wallets().get(evm.id);
  return {
    signer: {signerId: configured.signerId, policyIds: configured.policyIds},
    signerAuthorized: walletHasSigner(wallet, address, configured.signerId),
  };
}

const server = createServer(async (request, response) => {
  const verifyOnly = request.method === 'POST' && request.url === '/verify';
  const signOnboarding = request.method === 'POST' &&
    request.url === '/paradex/onboarding-signature';
  const signAuth = request.method === 'POST' && request.url === '/paradex/auth-signature';
  const checkSigner = request.method === 'POST' && request.url === '/paradex/signer-status';
  const registerSubkey = request.method === 'POST' &&
    request.url === '/paradex/subkey-registration-signature';
  const authSubkey = request.method === 'POST' && request.url === '/paradex/subkey-auth-signature';
  const signOrder = request.method === 'POST' && request.url === '/paradex/order-signature';
  const relayEvm = request.method === 'POST' && request.url === '/relay/evm-transaction';
  if (!verifyOnly && !signOnboarding && !signAuth && !checkSigner &&
      !registerSubkey && !authSubkey && !signOrder && !relayEvm) {
    response.writeHead(404).end();
    return;
  }
  let body = '';
  try {
    for await (const chunk of request) {
      body += chunk;
      if (body.length > 8192) throw new Error('request too large');
    }
    const input = JSON.parse(body);
    const accessToken = input.accessToken;
    if (typeof accessToken !== 'string' || !accessToken) throw new Error('token required');
    let claims;
    try {
      claims = await privy.utils().auth().verifyAccessToken(accessToken);
    } catch {
      send(response, 401, {error: 'invalid or expired Privy access token'});
      return;
    }
    const userId = claims.userId ?? claims.user_id;
    if (typeof userId !== 'string' || !userId.startsWith('did:privy:')) {
      throw new Error('invalid identity');
    }
    let user;
    try {
      user = await privy.users()._get(userId);
    } catch {
      send(response, 503, {error: 'Privy user lookup unavailable'});
      return;
    }
    const wallets = user.linked_accounts.filter((account) =>
      account.type === 'wallet' &&
      account.wallet_client_type === 'privy' &&
      account.connector_type === 'embedded'
    );
    const evm = wallets.find((account) => account.chain_type === 'ethereum');
    const evmWallet = evm?.address ?? null;
    const solanaWallet = wallets.find((account) => account.chain_type === 'solana')?.address ?? null;
    if (verifyOnly) {
      send(response, 200, {userId, evmWallet, solanaWallet});
      return;
    }
    if (typeof input.walletAddress !== 'string' ||
        evmWallet?.toLowerCase() !== input.walletAddress.toLowerCase()) {
      send(response, 403, {error: 'wallet does not belong to the signed-in user'});
      return;
    }
    // Sends a transaction the user already confirmed in the app, gas sponsored. The user's own session
    // authorizes it (no server signer involved); the engine only asks for transactions it planned.
    if (relayEvm) {
      const {chainId, to, data, idempotencyKey} = input;
      if (![8453, 84532].includes(chainId) || !/^0x[0-9a-fA-F]{40}$/.test(to ?? '') ||
          !/^0x([0-9a-fA-F]{2})*$/.test(data ?? '') || typeof idempotencyKey !== 'string' ||
          idempotencyKey.length < 8 || idempotencyKey.length > 200) {
        send(response, 400, {error: 'invalid relay request'});
        return;
      }
      try {
        const sent = await privy.wallets().ethereum().sendTransaction(evm.id, {
          caip2: `eip155:${chainId}`,
          params: {transaction: {to, data, value: '0x0', chain_id: chainId}},
          sponsor: true,
          idempotency_key: idempotencyKey,
          authorization_context: {user_jwts: [accessToken]},
        });
        send(response, 200, {userId, walletAddress: evmWallet, hash: sent.hash});
      } catch (error) {
        send(response, 502, {error: String(error?.message ?? error).slice(0, 200)});
      }
      return;
    }
    let status;
    try {
      status = await signerStatus(evm, evmWallet);
    } catch {
      send(response, 503, {error: 'Privy signer check unavailable'});
      return;
    }
    if (checkSigner) {
      send(response, 200, {userId, walletAddress: evmWallet, ...status});
      return;
    }
    if (!status.signerAuthorized) {
      send(response, 409, {error: 'embedded wallet has not granted server signing permission'});
      return;
    }
    const environment = process.env.PARADEX_ENV ?? 'prod';
    if (registerSubkey || authSubkey || signOrder) {
      const subkey = deriveTradeSubkey(signer.privateKey, environment, userId, evmWallet);
      if (registerSubkey) {
        const message = subkeyRegistrationMessage(environment, evmWallet, subkey.publicKey);
        const signed = await privy.wallets().ethereum().signMessage(evm.id, {
          message,
          authorization_context: {authorization_private_keys: [signer.privateKey]},
        });
        recoverOnboardingPublicKey(message, signed.signature, evmWallet);
        send(response, 200, {userId, walletAddress: evmWallet, publicKey: subkey.publicKey,
          signature: signed.signature, siweMessage: message});
        return;
      }
      const accountAddress = input.accountAddress;
      const chainId = input.chainId;
      if (authSubkey) {
        const proof = signSubkeyAuth(subkey, accountAddress, chainId,
          Math.floor(Date.now() / 1000));
        send(response, 200, {userId, walletAddress: evmWallet, publicKey: subkey.publicKey,
          ...proof});
        return;
      }
      const timestamp = Date.now();
      const signature = signParadexOrder(subkey, accountAddress, chainId, input.order, timestamp);
      send(response, 200, {userId, walletAddress: evmWallet, publicKey: subkey.publicKey,
        signature, timestamp});
      return;
    }
    const message = signOnboarding
      ? onboardingMessage(environment, evmWallet)
      : authMessage(environment, evmWallet);
    let signed;
    try {
      signed = await privy.wallets().ethereum().signMessage(evm.id, {
        message,
        authorization_context: {authorization_private_keys: [signer.privateKey]},
      });
    } catch {
      send(response, 409, {error: 'Privy could not sign with this wallet and policy'});
      return;
    }
    const publicKey = recoverOnboardingPublicKey(message, signed.signature, evmWallet);
    send(response, 200, {
      userId,
      walletAddress: evmWallet,
      signature: signed.signature,
      siweMessageBase64: Buffer.from(message, 'utf8').toString('base64'),
      publicKey,
    });
  } catch {
    send(response, 400, {error: 'invalid onboarding request'});
  }
});
server.listen(port, '127.0.0.1');
