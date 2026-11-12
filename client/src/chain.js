// Small helpers around near-api-js: provider, testnet faucet, reading results.

import { Account, JsonRpcProvider, KeyPair } from 'near-api-js';
import { AccountDoesNotExistError } from 'near-api-js/rpc-errors';

export const NETWORKS = {
  testnet: {
    rpc: 'https://test.rpc.fastnear.com',
    faucet: 'https://helper.testnet.near.org/account',
    explorer: 'https://testnet.nearblocks.io',
  },
};

/** RPC provider for testnet. Set NEAR_RPC_URL to use another endpoint. */
export function testnetProvider() {
  return new JsonRpcProvider({ url: process.env.NEAR_RPC_URL || NETWORKS.testnet.rpc });
}

export function txUrl(hash) {
  return `${NETWORKS.testnet.explorer}/txns/${hash}`;
}

export function accountUrl(accountId) {
  return `${NETWORKS.testnet.explorer}/address/${accountId}`;
}

export function newKeyPair() {
  const keyPair = KeyPair.fromRandom('ed25519');
  return { secretKey: keyPair.toString(), publicKey: keyPair.getPublicKey().toString() };
}

export function publicKeyOf(secretKey) {
  return KeyPair.fromString(secretKey).getPublicKey().toString();
}

/**
 * Create a funded testnet account through the public faucet (10 NEAR at the time of writing).
 * Returns { accountId, secretKey, publicKey }.
 */
export async function createFaucetAccount(prefix = 'leash') {
  const { secretKey, publicKey } = newKeyPair();
  const accountId = `${prefix}-${Date.now()}-${Math.floor(Math.random() * 1e6)}.testnet`;
  let lastError;
  for (let attempt = 1; attempt <= 3; attempt += 1) {
    try {
      const response = await fetch(NETWORKS.testnet.faucet, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ newAccountId: accountId, newAccountPublicKey: publicKey }),
      });
      const text = await response.text();
      if (response.ok) return { accountId, secretKey, publicKey };
      lastError = new Error(`faucet answered ${response.status}: ${text.slice(0, 200)}`);
    } catch (error) {
      lastError = error;
    }
    await sleep(2000 * attempt);
  }
  throw new Error(
    `Could not create a testnet account through the faucet (${lastError.message}). ` +
      'The faucet limits how many accounts one IP address can create per day. ' +
      'Set NEAR_ACCOUNT_ID and NEAR_PRIVATE_KEY to use an existing testnet account with at least 6 NEAR.',
  );
}

/**
 * The account that pays for a demo or test run: taken from NEAR_ACCOUNT_ID and
 * NEAR_PRIVATE_KEY when both are set, otherwise a fresh faucet account.
 */
export async function fundedAccount(provider, prefix) {
  const accountId = process.env.NEAR_ACCOUNT_ID;
  const secretKey = process.env.NEAR_PRIVATE_KEY;
  if (accountId && secretKey) {
    return { account: new Account(accountId, provider, secretKey), accountId, secretKey, fromFaucet: false };
  }
  const created = await createFaucetAccount(prefix);
  await waitForAccount(provider, created.accountId);
  return {
    account: new Account(created.accountId, provider, created.secretKey),
    accountId: created.accountId,
    secretKey: created.secretKey,
    fromFaucet: true,
  };
}

/**
 * Sub-account names for one run under `ownerId`.
 * A fresh owner gets plain names (`wallet.<owner>`, `shop.<owner>`). An owner that
 * already has a wallet from an earlier run gets a suffix, so a second run on the
 * same account does not collide with the first.
 * Returns { suffix, id(name) }.
 */
export async function runNames(provider, ownerId) {
  let suffix = '';
  try {
    await provider.viewAccount({ accountId: `wallet.${ownerId}`, finality: 'final' });
    suffix = `-${Date.now().toString(36)}`;
  } catch (error) {
    if (!(error instanceof AccountDoesNotExistError)) throw error;
  }
  return { suffix, id: (name) => `${name}${suffix}.${ownerId}` };
}

export async function waitForAccount(provider, accountId, timeoutMs = 30000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      return await provider.viewAccount({ accountId, finality: 'final' });
    } catch (error) {
      if (Date.now() > deadline) throw error;
      await sleep(1000);
    }
  }
}

/** The JSON value a transaction returned, or undefined when it returned nothing. */
export function returnValue(outcome) {
  const encoded = outcome?.status?.SuccessValue;
  if (encoded === undefined || encoded === null || encoded === '') return undefined;
  const text = Buffer.from(encoded, 'base64').toString('utf8');
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

/** Leash events (NEP-297 logs) found in a transaction outcome, in execution order. */
export function eventsOf(outcome) {
  const events = [];
  for (const receipt of outcome?.receipts_outcome ?? []) {
    for (const line of receipt.outcome?.logs ?? []) {
      if (!line.startsWith('EVENT_JSON:')) continue;
      try {
        const parsed = JSON.parse(line.slice('EVENT_JSON:'.length));
        if (parsed.standard === 'leash') events.push({ event: parsed.event, ...parsed.data?.[0] });
      } catch {
        // not ours
      }
    }
  }
  return events;
}

export function txHash(outcome) {
  return outcome?.transaction?.hash ?? outcome?.transaction_outcome?.id;
}

export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
