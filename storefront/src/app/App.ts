import { fetchCatalog } from "../services/catalog";
import { initClusterContext } from "../services/cluster";
import { fetchTokenMetadata } from "../services/metadata";
import { fetchSellerInventory } from "../services/inventory";
import { renderWalletHeader, initWalletBridge } from "./WalletHeader";
import { renderTokenShowcase, type EnrichedToken } from "./TokenCard";
import { openPurchaseSheet } from "./PurchaseSheet";
import type { CatalogItem } from "../services/catalog";

function mountWalletWhenReady(): void {
  const tryMount = () => {
    if (typeof window.MountWalletAdapter === "function") {
      window.MountWalletAdapter();
      return;
    }
    setTimeout(tryMount, 100);
  };
  tryMount();
}

export async function bootstrapApp(root: HTMLElement): Promise<void> {
  initWalletBridge();
  mountWalletWhenReady();

  root.innerHTML = `<div class="shell"><p class="loading">Loading catalog…</p></div>`;

  try {
    const catalog = await fetchCatalog("");
    const connection = await initClusterContext(catalog);
    if (typeof window.RemountStorefrontWallet === "function") {
      window.RemountStorefrontWallet();
    }

    const shell = document.createElement("div");
    shell.className = "shell";

    const headerMount = document.createElement("div");
    renderWalletHeader(headerMount, catalog);

    const hero = document.createElement("section");
    hero.className = "hero";
    hero.innerHTML = `
      <h2>Buy SPL tokens with x402</h2>
      <p>Pay USDC via SLA-Escrow. Tokens delivered on-chain after oracle-verified transfer — the same rail AI agents use, built for humans too.</p>
    `;

    const showcase = document.createElement("div");
    showcase.className = "loading";
    showcase.textContent = "Fetching on-chain metadata & inventory…";

    shell.append(headerMount, hero, showcase);
    root.replaceChildren(shell);

    const enriched: EnrichedToken[] = await Promise.all(
      catalog.items.map(async (item) => {
        const [metadata, inventory] = await Promise.all([
          fetchTokenMetadata(connection, item.mint),
          fetchSellerInventory(connection, item, catalog.sellerPubkey),
        ]);
        return { item, metadata, inventory };
      }),
    );

    showcase.className = "";
    showcase.textContent = "";

    const onBuy = (item: CatalogItem) => {
      openPurchaseSheet(catalog, item, () => {
        void refreshInventory(showcase, catalog, connection, enriched, onBuy);
      });
    };

    renderTokenShowcase(showcase, catalog, enriched, onBuy);
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    root.innerHTML = `<div class="shell"><div class="error-banner">Failed to load shop: ${msg}</div></div>`;
  }
}

async function refreshInventory(
  showcase: HTMLElement,
  catalog: Awaited<ReturnType<typeof fetchCatalog>>,
  connection: Awaited<ReturnType<typeof initClusterContext>>,
  enriched: EnrichedToken[],
  onBuy: (item: CatalogItem) => void,
): Promise<void> {
  for (let i = 0; i < enriched.length; i++) {
    enriched[i].inventory = await fetchSellerInventory(
      connection,
      enriched[i].item,
      catalog.sellerPubkey,
    );
  }
  renderTokenShowcase(showcase, catalog, enriched, onBuy);
}
