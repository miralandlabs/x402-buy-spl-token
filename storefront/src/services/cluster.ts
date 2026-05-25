import { Connection } from "@solana/web3.js";
import type { CatalogDocument } from "./catalog";
import { defaultPublicRpcForCluster } from "./clusterConstants";

declare global {
  interface Window {
    __STOREFRONT_RPC__?: string;
  }
}

export async function resolveRpcEndpoint(catalog: CatalogDocument): Promise<string> {
  if (catalog.rpcUrl) {
    return catalog.rpcUrl;
  }

  try {
    const healthUrl = catalog.facilitatorUrl.replace(/\/$/, "") + "/health";
    const res = await fetch(healthUrl);
    if (res.ok) {
      const data = (await res.json()) as { solanaWalletRpcUrl?: string };
      if (data.solanaWalletRpcUrl) {
        return data.solanaWalletRpcUrl;
      }
    }
  } catch {
    /* fallback below */
  }

  return defaultPublicRpcForCluster(catalog.cluster);
}

export async function initClusterContext(catalog: CatalogDocument): Promise<Connection> {
  const rpc = await resolveRpcEndpoint(catalog);
  window.__STOREFRONT_RPC__ = rpc;
  return new Connection(rpc, "confirmed");
}

export function solscanTxUrl(signature: string, cluster: string): string {
  const base = `https://solscan.io/tx/${signature}`;
  if (cluster === "devnet") return `${base}?cluster=devnet`;
  if (cluster === "testnet") return `${base}?cluster=testnet`;
  return base;
}

export function solscanTokenUrl(mint: string, cluster: string): string {
  const base = `https://solscan.io/token/${mint}`;
  if (cluster === "devnet") return `${base}?cluster=devnet`;
  if (cluster === "testnet") return `${base}?cluster=testnet`;
  return base;
}
