import {createServer} from 'node:http';
import {PrivyClient} from '@privy-io/node';
import {authMessage, onboardingMessage, recoverOnboardingPublicKey} from './paradex-onboarding.mjs';

const appId = process.env.PRIVY_APP_ID;
const appSecret = process.env.PRIVY_APP_SECRET;
if (!appId || !appSecret) throw new Error('PRIVY_APP_ID and PRIVY_APP_SECRET are required');
const privy = new PrivyClient({appId, appSecret});
const port = Number(process.env.PRIVY_BRIDGE_PORT ?? 3101);
if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('invalid bridge port');

function send(response, status, body) {
  response.writeHead(status, {'content-type': 'application/json'});
  response.end(JSON.stringify(body));
}

const server = createServer(async (request, response) => {
  const verifyOnly = request.method === 'POST' && request.url === '/verify';
  const signOnboarding = request.method === 'POST' &&
    request.url === '/paradex/onboarding-signature';
  const signAuth = request.method === 'POST' && request.url === '/paradex/auth-signature';
  if (!verifyOnly && !signOnboarding && !signAuth) {
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
    if (!evm.delegated || typeof evm.id !== 'string' || !evm.id) {
      send(response, 409, {error: 'embedded wallet has not granted server signing permission'});
      return;
    }
    const environment = process.env.PARADEX_ENV ?? 'prod';
    const message = signOnboarding
      ? onboardingMessage(environment, evmWallet)
      : authMessage(environment, evmWallet);
    let signed;
    try {
      signed = await privy.wallets().ethereum().signMessage(evm.id, {message});
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
