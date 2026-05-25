import { Connection, PublicKey } from "@solana/web3.js";
import { getAssociatedTokenAddress, getAccount } from "@solana/spl-token";
import type { CatalogItem } from "./catalog";

export interface InventoryInfo {
  rawAmount: bigint;
  uiAmount: string;
  inStock: boolean;
}

export async function fetchSellerInventory(
  connection: Connection,
  item: CatalogItem,
  sellerPubkey: string,
): Promise<InventoryInfo> {
  try {
    let ata: PublicKey;
    if (item.senderTreasuryAta) {
      ata = new PublicKey(item.senderTreasuryAta);
    } else {
      const mint = new PublicKey(item.mint);
      const seller = new PublicKey(sellerPubkey);
      ata = await getAssociatedTokenAddress(mint, seller);
    }

    const account = await getAccount(connection, ata);
    const raw = account.amount;
    const ui = formatUiAmount(raw, item.decimals);
    return {
      rawAmount: raw,
      uiAmount: ui,
      inStock: raw > 0n,
    };
  } catch {
    return { rawAmount: 0n, uiAmount: "0", inStock: false };
  }
}

function formatUiAmount(raw: bigint, decimals: number): string {
  if (decimals === 0) return raw.toString();
  const s = raw.toString().padStart(decimals + 1, "0");
  const intPart = s.slice(0, -decimals) || "0";
  const frac = s.slice(-decimals).replace(/0+$/, "");
  return frac ? `${intPart}.${frac}` : intPart;
}
