import {createServer} from 'node:http';
import {createHash, randomUUID} from 'node:crypto';
import {PrivyClient} from '@privy-io/node';
import {authMessage, onboardingMessage, subkeyRegistrationMessage, recoverOnboardingPublicKey} from './paradex-onboarding.mjs';
import {deriveTradeSubkey, signSubkeyAuth, signParadexOrder} from './trade-subkey.mjs';
import {quoteSwap, swapFromSui, prepareSuiCashout, transferSui} from './sui-swap.mjs';
import {checkSignature, signable} from './base-authorization.mjs';

const SUI_COIN_TYPE = /^0x[0-9a-fA-F]{1,64}::[A-Za-z_][A-Za-z0-9_]*::[A-Za-z_][A-Za-z0-9_]*$/;
const POSITIVE_INTEGER = /^[1-9][0-9]{0,30}$/;
const preparedCashouts = new Map();
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

async function ensureReceivingWallet(userId, chainType) {
  const externalId = `atlas_${chainType}_${createHash('sha256').update(userId).digest('hex').slice(0, 32)}`;
  for await (const wallet of privy.wallets().list({external_id: externalId})) {
    if (wallet.chain_type !== chainType || !wallet.address) {
      throw new Error('Atlas receiving wallet does not match its chain');
    }
    return wallet;
  }
  return privy.wallets().create({
    chain_type: chainType,
    owner: {user_id: userId},
    external_id: externalId,
    display_name: `Atlas ${chainType.toUpperCase()}`,
    idempotency_key: externalId,
  });
}

const server = createServer(async (request, response) => {
  const verifyOnly = request.method === 'POST' && request.url === '/verify';
  const ensureWallet = request.method === 'POST' && request.url === '/wallet/ensure';
  const signOnboarding = request.method === 'POST' &&
    request.url === '/paradex/onboarding-signature';
  const signAuth = request.method === 'POST' && request.url === '/paradex/auth-signature';
  const checkSigner = request.method === 'POST' && request.url === '/paradex/signer-status';
  const registerSubkey = request.method === 'POST' &&
    request.url === '/paradex/subkey-registration-signature';
  const authSubkey = request.method === 'POST' && request.url === '/paradex/subkey-auth-signature';
  const signOrder = request.method === 'POST' && request.url === '/paradex/order-signature';
  const relayEvm = request.method === 'POST' && request.url === '/relay/evm-transaction';
  const signAuthorization = request.method === 'POST' && request.url === '/evm/sign-authorization';
  const suiQuote = request.method === 'POST' && request.url === '/sui/quote';
  const suiSwap = request.method === 'POST' && request.url === '/sui/swap';
  const suiCashoutPrepare = request.method === 'POST' && request.url === '/sui/cashout/prepare';
  const suiCashoutCommit = request.method === 'POST' && request.url === '/sui/cashout/commit';
  if (!verifyOnly && !ensureWallet && !signOnboarding && !signAuth && !checkSigner &&
      !registerSubkey && !authSubkey && !signOrder && !relayEvm && !signAuthorization && !suiQuote &&
      !suiSwap && !suiCashoutPrepare && !suiCashoutCommit) {
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
    // A cashout can only land in the verified user's own Base wallet. Prepare stores the
    // venue-issued deposit address so the engine can persist it before commit moves SUI.
    if (suiCashoutPrepare || suiCashoutCommit) {
      try {
        const wallet = await ensureReceivingWallet(userId, 'sui');
        if (suiCashoutPrepare) {
          if (!evmWallet || !POSITIVE_INTEGER.test(String(input.amount ?? '')) ||
              !POSITIVE_INTEGER.test(String(input.minimumOut ?? ''))) {
            send(response, 400, {error: 'invalid cashout request'});
            return;
          }
          const quote = await prepareSuiCashout({
            wallet, evmWallet, amount: input.amount, minimumOut: input.minimumOut,
          });
          for (const [id, prepared] of preparedCashouts) {
            if (prepared.expires <= Date.now()) preparedCashouts.delete(id);
          }
          if (preparedCashouts.size >= 1000) throw new Error('cashout queue is full');
          const cashoutId = randomUUID();
          preparedCashouts.set(cashoutId, {userId, walletId: wallet.id, address: wallet.address,
            amount: input.amount, depositAddress: quote.depositAddress,
            expires: Date.now() + 180_000});
          send(response, 200, {userId, address: wallet.address, cashoutId, ...quote});
          return;
        }
        const prepared = preparedCashouts.get(input.cashoutId);
        if (!prepared || prepared.userId !== userId || prepared.walletId !== wallet.id ||
            prepared.address !== wallet.address || prepared.expires <= Date.now()) {
          send(response, 409, {error: 'cashout preparation expired'});
          return;
        }
        preparedCashouts.delete(input.cashoutId); // one use, even if the network response is lost
        const result = await transferSui({
          wallet, recipient: prepared.depositAddress, amount: prepared.amount, reserve: '20000000',
          rawSign: async (hex) => {
            const signed = await privy.wallets().rawSign(wallet.id, {
              params: {bytes: hex, encoding: 'hex', hash_function: 'blake2b256'},
              authorization_context: {user_jwts: [accessToken]},
            });
            return signed.signature;
          },
        });
        send(response, 200, {userId, address: wallet.address, ...result});
      } catch (error) {
        send(response, 502, {error: `Cashout unavailable: ${error.message}`});
      }
      return;
    }
    // Sui coins: price SUI → coin, and swap SUI in the user's own Sui wallet with their authorization.
    if (suiQuote || suiSwap) {
      if (!SUI_COIN_TYPE.test(input.coinType ?? '') || !POSITIVE_INTEGER.test(String(input.amount ?? ''))) {
        send(response, 400, {error: 'invalid Sui swap request'});
        return;
      }
      let wallet;
      try {
        wallet = await ensureReceivingWallet(userId, 'sui');
      } catch {
        send(response, 503, {error: 'Privy Sui wallet unavailable'});
        return;
      }
      try {
        if (suiQuote) {
          const router = await quoteSwap(input.coinType, input.amount, wallet.address);
          send(response, 200, {userId, address: wallet.address, amountOut: router.amountOut.toString()});
          return;
        }
        const reserve = POSITIVE_INTEGER.test(String(input.reserve ?? '')) ? input.reserve : '0';
        const result = await swapFromSui({
          wallet,
          coinType: input.coinType,
          amount: input.amount,
          reserve,
          rawSign: async (hex) => {
            const signed = await privy.wallets().rawSign(wallet.id, {
              params: {bytes: hex, encoding: 'hex', hash_function: 'blake2b256'},
              authorization_context: {user_jwts: [accessToken]},
            });
            return signed.signature;
          },
        });
        send(response, 200, {userId, address: wallet.address, ...result});
      } catch (error) {
        send(response, 502, {error: `Sui swap unavailable: ${error.message}`});
      }
      return;
    }
    if (ensureWallet) {
      if (!['sui', 'near'].includes(input.chainType)) {
        send(response, 400, {error: 'unsupported receiving wallet chain'});
        return;
      }
      try {
        const wallet = await ensureReceivingWallet(userId, input.chainType);
        send(response, 200, {userId, chainType: input.chainType, address: wallet.address});
      } catch {
        send(response, 503, {error: 'Privy receiving wallet unavailable'});
      }
      return;
    }
    if (typeof input.walletAddress !== 'string' ||
        evmWallet?.toLowerCase() !== input.walletAddress.toLowerCase()) {
      send(response, 403, {error: 'wallet does not belong to the signed-in user'});
      return;
    }
    // Sends a transaction the user already confirmed in the app. The user's own session authorizes
    // it (no server signer involved); the engine only asks for transactions it planned. The wallet
    // pays its own gas: Privy sponsorship is never requested.
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
          sponsor: false,
          idempotency_key: idempotencyKey,
          authorization_context: {user_jwts: [accessToken]},
        });
        send(response, 200, {userId, walletAddress: evmWallet, hash: sent.hash});
      } catch (error) {
        send(response, 502, {error: String(error?.message ?? error).slice(0, 200)});
      }
      return;
    }
    // Base without gas, for a plan the user already confirmed in the app: their own session signs a
    // venue's deposit authorization or a CoW gas top-up, and nothing wider (base-authorization.mjs).
    if (signAuthorization) {
      let typedData;
      try {
        typedData = signable(input.typedData, evmWallet);
      } catch (error) {
        send(response, 400, {error: `invalid authorization: ${error.message}`});
        return;
      }
      try {
        const signed = await privy.wallets().ethereum().signTypedData(evm.id, {
          params: {typed_data: typedData},
          authorization_context: {user_jwts: [accessToken]},
        });
        checkSignature(typedData, signed.signature, evmWallet);
        send(response, 200, {userId, walletAddress: evmWallet, signature: signed.signature});
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
