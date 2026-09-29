import {readFileSync, writeFileSync, existsSync, chmodSync} from 'node:fs';
import {fileURLToPath} from 'node:url';
import {PrivyClient, generateP256KeyPair} from '@privy-io/node';

const credentialsPath = fileURLToPath(new URL('../../../.env.privy', import.meta.url));
const outputPath = fileURLToPath(new URL('../../../.env.privy-signer', import.meta.url));

function readSettings(path) {
  if (!existsSync(path)) return {};
  return Object.fromEntries(readFileSync(path, 'utf8').split(/\r?\n/)
    .filter((line) => /^[A-Z][A-Z0-9_]*=/.test(line))
    .map((line) => {
      const split = line.indexOf('=');
      const raw = line.slice(split + 1).trim();
      const value = raw.startsWith('"') && raw.endsWith('"') ? raw.slice(1, -1) : raw;
      return [line.slice(0, split), value];
    }));
}

const credentials = readSettings(credentialsPath);
const appId = process.env.PRIVY_APP_ID ?? credentials.PRIVY_APP_ID;
const appSecret = process.env.PRIVY_APP_SECRET ?? credentials.PRIVY_APP_SECRET;
if (!appId || !appSecret) throw new Error('Privy app credentials are missing');
const privy = new PrivyClient({appId, appSecret});

let saved = readSettings(outputPath);
if (!saved.PRIVY_SERVER_AUTH_PRIVATE_KEY || !saved.PRIVY_SERVER_AUTH_PUBLIC_KEY) {
  if (existsSync(outputPath)) throw new Error('incomplete signer file; inspect it before retrying');
  const pair = await generateP256KeyPair();
  saved = {
    PRIVY_SERVER_AUTH_PRIVATE_KEY: pair.privateKey,
    PRIVY_SERVER_AUTH_PUBLIC_KEY: pair.publicKey,
  };
  writeFileSync(outputPath,
    'PRIVY_SERVER_AUTH_PRIVATE_KEY=' + pair.privateKey + '\n' +
    'PRIVY_SERVER_AUTH_PUBLIC_KEY=' + pair.publicKey + '\n', {mode: 0o600, flag: 'wx'});
}
chmodSync(outputPath, 0o600);

if (!saved.PRIVY_SERVER_SIGNER_ID) {
  const quorum = await privy.keyQuorums().create({
    display_name: 'Atlas Engine Perps Testnet',
    authorization_threshold: 1,
    public_keys: [saved.PRIVY_SERVER_AUTH_PUBLIC_KEY],
  });
  saved.PRIVY_SERVER_SIGNER_ID = quorum.id;
  writeFileSync(outputPath,
    'PRIVY_SERVER_SIGNER_ID=' + quorum.id + '\n' +
    'PRIVY_SERVER_AUTH_PRIVATE_KEY=' + saved.PRIVY_SERVER_AUTH_PRIVATE_KEY + '\n' +
    'PRIVY_SERVER_AUTH_PUBLIC_KEY=' + saved.PRIVY_SERVER_AUTH_PUBLIC_KEY + '\n' +
    'PRIVY_SERVER_SIGNER_POLICY_IDS=\n', {mode: 0o600});
}
const verified = await privy.keyQuorums().get(saved.PRIVY_SERVER_SIGNER_ID);
if (verified.authorization_threshold !== 1 ||
    !verified.authorization_keys.some((key) =>
      key.public_key === saved.PRIVY_SERVER_AUTH_PUBLIC_KEY)) {
  throw new Error('saved signer does not match its Privy key quorum');
}
console.log('Privy server signer ID: ' + saved.PRIVY_SERVER_SIGNER_ID);
console.log('Private authorization key saved only in ignored .env.privy-signer');
