import {createServer} from 'node:http';
import {PrivyClient} from '@privy-io/node';

const appId = process.env.PRIVY_APP_ID;
const appSecret = process.env.PRIVY_APP_SECRET;
if (!appId || !appSecret) throw new Error('PRIVY_APP_ID and PRIVY_APP_SECRET are required');
const privy = new PrivyClient({appId, appSecret});
const port = Number(process.env.PRIVY_BRIDGE_PORT ?? 3101);
if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('invalid bridge port');

const server = createServer(async (request, response) => {
  if (request.method !== 'POST' || request.url !== '/verify') {
    response.writeHead(404).end();
    return;
  }
  let body = '';
  try {
    for await (const chunk of request) {
      body += chunk;
      if (body.length > 8192) throw new Error('request too large');
    }
    const {accessToken} = JSON.parse(body);
    if (typeof accessToken !== 'string' || !accessToken) throw new Error('token required');
    const claims = await privy.utils().auth().verifyAccessToken(accessToken);
    const userId = claims.userId ?? claims.user_id;
    if (typeof userId !== 'string' || !userId.startsWith('did:privy:')) {
      throw new Error('invalid identity');
    }
    response.writeHead(200, {'content-type': 'application/json'});
    response.end(JSON.stringify({userId}));
  } catch {
    response.writeHead(401, {'content-type': 'application/json'});
    response.end(JSON.stringify({error: 'invalid or expired Privy access token'}));
  }
});
server.listen(port, '127.0.0.1');
