export interface CatalogItem {
  mint: string;
  decimals: number;
  name: string;
  priceUsdcUi: string;
  deliverAmountUi: string;
  senderTreasuryAta?: string | null;
}

export interface CatalogDocument {
  contractVersion: string;
  network: string;
  cluster: string;
  usdcMint: string;
  facilitatorUrl: string;
  sellerPubkey: string;
  intentContractUrl: string;
  rpcUrl?: string | null;
  items: CatalogItem[];
}

let cached: CatalogDocument | null = null;

export async function fetchCatalog(baseUrl = ""): Promise<CatalogDocument> {
  const res = await fetch(`${baseUrl}/api/v1/buy-spl-token/catalog`);
  if (!res.ok) {
    const text = await res.text();
    throw new Error(`Catalog fetch failed (${res.status}): ${text}`);
  }
  const doc = (await res.json()) as CatalogDocument;
  cached = doc;
  return doc;
}

export function getCatalog(): CatalogDocument | null {
  return cached;
}

export function clusterLabel(cluster: string): string {
  if (cluster === "devnet") return "Devnet";
  if (cluster === "testnet") return "Testnet";
  return "Mainnet";
}
