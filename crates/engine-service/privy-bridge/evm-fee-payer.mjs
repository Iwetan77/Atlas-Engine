// Atlas's EVM fee payer: an app-owned Privy wallet (no user owner; only Atlas's Privy app can sign for it),
// funded by Atlas, that sends a user exactly the gas a sale needs on Robinhood Chain right before the phone
// sends it. Users keep no ETH there (Agba: no reserves). One address serves every EVM chain.
import {createPublicClient, http, isAddress, toHex} from 'viem';

export const CHAINS = {
  robinhood: {
    id: 4663,
    rpc: process.env.ATLAS_ROBINHOOD_RPC_URL || 'https://rpc.mainnet.chain.robinhood.com',
    // The most one top-up may send: a few sales' worth of gas (a sale's most is about 0.00002 ETH).
    cap: 100_000_000_000_000n, // 0.0001 ETH
  },
};
// How often one user can be topped up: enough for real selling, not for draining the fee payer.
const PER_HOUR = 6;
const recent = new Map();

export function checkTopUp({chain, to, amount, owner, userId, now = Date.now()}) {
  const config = CHAINS[chain];
  if (!config) throw new Error('unsupported chain');
  if (!isAddress(to ?? '') || !isAddress(owner ?? '') || to.toLowerCase() !== owner.toLowerCase()) {
    throw new Error('gas goes only to your own wallet');
  }
  if (!/^[1-9][0-9]{0,30}$/.test(String(amount ?? ''))) throw new Error('invalid gas amount');
  const value = BigInt(amount);
  if (value > config.cap) throw new Error('gas amount too large');
  const times = (recent.get(userId) ?? []).filter((t) => now - t < 3_600_000);
  if (times.length >= PER_HOUR) throw new Error('too many gas top-ups; try again later');
  return {config, value, times};
}

export function recordTopUp(userId, times = recent.get(userId) ?? [], now = Date.now()) {
  recent.set(userId, [...times.filter((t) => now - t < 3_600_000), now]);
}

async function ensureWallet(privy) {
  const externalId = 'atlas_fee_payer_ethereum';
  for await (const wallet of privy.wallets().list({external_id: externalId})) {
    if (wallet.chain_type !== 'ethereum' || !wallet.address) throw new Error('Atlas fee payer does not match its chain');
    return wallet;
  }
  return privy.wallets().create({chain_type: 'ethereum', external_id: externalId,
    display_name: 'Atlas EVM fee payer', idempotency_key: externalId});
}
let feePayer;

// Sends `amount` wei of the chain's gas coin from the fee payer to the user's own wallet and waits for it
// to land. Nothing else can be sent: a plain transfer, capped, to the signed-in user's wallet only.
export async function topUpGas(privy, {chain, to, amount, owner, userId}) {
  const {config, value, times} = checkTopUp({chain, to, amount, owner, userId});
  feePayer ??= await ensureWallet(privy);
  const client = createPublicClient({transport: http(config.rpc, {timeout: 15_000})});
  const [nonce, fees, balance] = await Promise.all([
    client.getTransactionCount({address: feePayer.address, blockTag: 'pending'}),
    client.estimateFeesPerGas(),
    client.getBalance({address: feePayer.address}),
  ]);
  const gas = 21_000n;
  if (balance < value + gas * fees.maxFeePerGas) throw new Error('Atlas fee payer is empty');
  const {signed_transaction} = await privy.wallets().ethereum().signTransaction(feePayer.id, {params: {transaction: {
    type: 2, chain_id: config.id, to, value: toHex(value), nonce, gas_limit: toHex(gas),
    max_fee_per_gas: toHex(fees.maxFeePerGas), max_priority_fee_per_gas: toHex(fees.maxPriorityFeePerGas),
  }}});
  recordTopUp(userId, times);
  const hash = await client.sendRawTransaction({serializedTransaction: signed_transaction});
  const receipt = await client.waitForTransactionReceipt({hash, timeout: 30_000});
  if (receipt.status !== 'success') throw new Error('gas top-up failed');
  return {hash};
}

// The fee payer's address and what it holds on each chain, for /health.
export async function feePayerState(privy) {
  feePayer ??= await ensureWallet(privy);
  const balances = {};
  for (const [name, config] of Object.entries(CHAINS)) {
    try {
      const client = createPublicClient({transport: http(config.rpc, {timeout: 8_000})});
      balances[name] = (await client.getBalance({address: feePayer.address})).toString();
    } catch {
      balances[name] = null;
    }
  }
  return {address: feePayer.address, balances};
}
