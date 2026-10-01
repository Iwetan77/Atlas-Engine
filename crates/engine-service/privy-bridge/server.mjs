import {createServer} from 'node:http';
import {createHash, randomUUID} from 'node:crypto';
import {PrivyClient} from '@privy-io/node';
import {quoteSwap, swapFromSui, prepareSuiCashout, transferSui} from './sui-swap.mjs';
import {checkSignature, signable, signForEscrow} from './base-authorization.mjs';
import {agentFor, approveAction, approveTypedData, checkApproval, leverageAction, moveAction, orderAction,
  post as hlPost, signAsAgent} from './hyperliquid.mjs';
import {cashOut} from './hyperliquid-cashout.mjs';

const SUI_COIN_TYPE = /^0x[0-9a-fA-F]{1,64}::[A-Za-z_][A-Za-z0-9_]*::[A-Za-z_][A-Za-z0-9_]*$/;
const POSITIVE_INTEGER = /^[1-9][0-9]{0,30}$/;
const preparedCashouts = new Map();

const appId = process.env.PRIVY_APP_ID;
const appSecret = process.env.PRIVY_APP_SECRET;
if (!appId || !appSecret) throw new Error('PRIVY_APP_ID and PRIVY_APP_SECRET are required');
const privy = new PrivyClient({appId, appSecret});
const port = Number(process.env.PRIVY_BRIDGE_PORT ?? 3101);
if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('invalid bridge port');

// Hyperliquid wants a fresh nonce per action: milliseconds, never repeated even within one.
let lastNonce = 0;
function nextNonce() {
  lastNonce = Math.max(Date.now(), lastNonce + 1);
  return lastNonce;
}

function send(response, status, body) {
  response.writeHead(status, {'content-type': 'application/json'});
  response.end(JSON.stringify(body));
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
  const signAuthorization = request.method === 'POST' && request.url === '/evm/sign-authorization';
  const signEscrow = request.method === 'POST' && request.url === '/escrow/sign-authorization';
  const hlRoute = request.method === 'POST' && request.url.startsWith('/hyperliquid/') ?
    request.url.slice('/hyperliquid/'.length) : null;
  const hyperliquid = ['agent', 'approve', 'leverage', 'order', 'move', 'cashout'].includes(hlRoute);
  const suiQuote = request.method === 'POST' && request.url === '/sui/quote';
  const suiSwap = request.method === 'POST' && request.url === '/sui/swap';
  const suiCashoutPrepare = request.method === 'POST' && request.url === '/sui/cashout/prepare';
  const suiCashoutCommit = request.method === 'POST' && request.url === '/sui/cashout/commit';
  if (!verifyOnly && !ensureWallet && !signAuthorization && !signEscrow && !hyperliquid && !suiQuote &&
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
    // Signing as the user: Privy exchanges the user's identity token (not the access token) for a
    // key to their wallets. Only the same user's identity token is used; else the access token.
    let userJwt = accessToken;
    if (typeof input.identityToken === 'string' && input.identityToken) {
      try {
        const identity = await privy.utils().auth().verifyIdentityToken(input.identityToken);
        if (identity?.id === userId) userJwt = input.identityToken;
      } catch {
        // An expired or foreign identity token: fall back to the access token.
      }
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
    // Claiming an Atlas Link: the claimer is signed in, and the link's secret pays out its escrow,
    // only through a pinned receiver (base-authorization.mjs).
    if (signEscrow) {
      try {
        const signed = signForEscrow(input.typedData, input.secret);
        send(response, 200, {userId, escrow: signed.address, signature: signed.signature});
      } catch (error) {
        send(response, 400, {error: `invalid link payout: ${error.message}`});
      }
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
              authorization_context: {user_jwts: [userJwt]},
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
              authorization_context: {user_jwts: [userJwt]},
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
          authorization_context: {user_jwts: [userJwt]},
        });
        checkSignature(typedData, signed.signature, evmWallet);
        send(response, 200, {userId, walletAddress: evmWallet, signature: signed.signature});
      } catch (error) {
        send(response, 502, {error: String(error?.message ?? error).slice(0, 200)});
      }
      return;
    }
    // Hyperliquid, for the user's own account (their EVM wallet): their Atlas agent signs trades; their
    // own session signs only the one-time approval of that agent (hyperliquid.mjs) and a cash-out to
    // their own Solana or Base wallet (hyperliquid-cashout.mjs).
    if (hyperliquid) {
      const agent = agentFor(appSecret, userId, evmWallet);
      const answer = (result) => send(response, 200, {userId, walletAddress: evmWallet, agentAddress: agent.address, result});
      try {
        if (hlRoute === 'agent') return answer(null);
        if (hlRoute === 'cashout') {
          // Paid out only to this user's own wallet, never to an address the engine passes.
          const recipient = {solana: solanaWallet, base: evmWallet}[input.to];
          if (!recipient) {
            send(response, 400, {error: 'no wallet to pay out to'});
            return;
          }
          return answer(await cashOut({
            wallet: evmWallet, recipient, to: input.to, amount: String(input.amount ?? ''),
            relayKey: process.env.RELAY_API_KEY,
            signTyped: async (typed) => (await privy.wallets().ethereum().signTypedData(evm.id, {
              params: {typed_data: typed},
              authorization_context: {user_jwts: [userJwt]},
            })).signature,
            post: hlPost,
          }));
        }
        const nonce = nextNonce();
        if (hlRoute === 'approve') {
          const signed = await privy.wallets().ethereum().signTypedData(evm.id, {
            params: {typed_data: approveTypedData(agent.address, nonce)},
            authorization_context: {user_jwts: [userJwt]},
          });
          const signature = checkApproval(agent.address, nonce, signed.signature, evmWallet);
          return answer(await hlPost({action: approveAction(agent.address, nonce), nonce, signature}));
        }
        const action = hlRoute === 'order' ? orderAction(input) :
          hlRoute === 'move' ? moveAction({wallet: evmWallet, from: input.from, to: input.to, amount: input.amount}, nonce) :
          leverageAction(input);
        return answer(await hlPost({action, nonce, signature: signAsAgent(agent.key, action, nonce), vaultAddress: null}));
      } catch (error) {
        send(response, 502, {error: String(error?.message ?? error).slice(0, 200)});
        return;
      }
    }
  } catch {
    send(response, 400, {error: 'invalid request'});
  }
});
server.listen(port, '127.0.0.1');
