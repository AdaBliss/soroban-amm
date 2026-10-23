import { AmmPool, FactoryClient, TokenClient } from "@soroban-amm/sdk";
import { parseAmountIn, parseDeadlineSeconds, parseSlippageBps } from "./config.js";

const rpcUrl = process.env.STELLAR_RPC_URL ?? "https://soroban-testnet.stellar.org";
const networkPassphrase = process.env.STELLAR_NETWORK_PASSPHRASE ?? "Test SDF Network ; September 2015";
const poolId = required("AMM_CONTRACT_ID");
const tokenInId = required("TOKEN_IN_CONTRACT_ID");
const sourceAddress = required("SOURCE_ADDRESS");
const lpTokenId = process.env.LP_TOKEN_CONTRACT_ID;

function required(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required; see examples/client/README.md`);
  return value;
}

function config(contractId: string) {
  return { rpcUrl, networkPassphrase, contractId };
}

async function main(): Promise<void> {
  const pool = new AmmPool(config(poolId));
  const token = new TokenClient(config(tokenInId));
  const factoryId = process.env.FACTORY_CONTRACT_ID;
  const factory = factoryId ? new FactoryClient(config(factoryId)) : undefined;

  let amountIn: bigint;
  let deadlineSeconds: number;
  let slippageBps: bigint;
  try {
    amountIn = parseAmountIn(process.env.SWAP_AMOUNT_IN);
    deadlineSeconds = parseDeadlineSeconds(process.env.DEADLINE_SECONDS);
    slippageBps = parseSlippageBps(process.env.SLIPPAGE_BPS);
  } catch (error: unknown) {
    const message = error instanceof Error ? error.message : String(error);
    console.error(`Invalid configuration: ${message}`);
    process.exitCode = 1;
    return;
  }
  const deadline = BigInt(Math.floor(Date.now() / 1000) + deadlineSeconds);

  // 1. Read pool info to confirm the pair and current reserves.
  const info = await pool.getInfo();
  console.log("Pool", poolId, "tokens", info.tokenA, info.tokenB);

  // 2. Quote before submitting; bigint preserves token precision and price impact.
  const quote = await pool.simulateSwap(tokenInId, amountIn);
  const minAmountOut = quote.amountOut * (10_000n - slippageBps) / 10_000n;
  console.log("Quote", { amountIn: quote.amountIn, amountOut: quote.amountOut, priceImpactBps: quote.priceImpactBps, minAmountOut, deadline });

  // 3. The current SDK has no signed AmmPool.submitSwap method. Keep the reviewed
  // parameters here rather than reintroducing hand-rolled Contract/TransactionBuilder code.
  console.log("Swap execution parameters", { trader: sourceAddress, tokenIn: tokenInId, amountIn, minAmountOut, deadline });

  // 4. Add liquidity with the same deadline/slippage discipline through the wallet adapter.
  console.log("Add-liquidity review", { provider: sourceAddress, deadline, token: await token.symbol() });

  // 5. Read the LP token balance after the add-liquidity transaction confirms.
  if (lpTokenId) console.log("LP shares", await new TokenClient(config(lpTokenId)).balance(sourceAddress));
  else console.log("Set LP_TOKEN_CONTRACT_ID to read LP shares.");

  // 6. Remove liquidity with a fresh deadline after reviewing returned amounts.
  console.log("Remove-liquidity review", { provider: sourceAddress, deadline: BigInt(Math.floor(Date.now() / 1000) + 300) });
  if (factory) console.log("Known pools", await factory.allPools());
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : String(error);
  console.error(`AMM contract call failed: ${message}. See docs/error-codes.md for recovery guidance.`);
  process.exitCode = 1;
});
