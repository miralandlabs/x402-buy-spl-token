import type { CatalogDocument, CatalogItem } from "../services/catalog";
import type { TokenMetadata } from "../services/metadata";
import { monogramFromMint } from "../services/metadata";
import type { InventoryInfo } from "../services/inventory";

export interface EnrichedToken {
  item: CatalogItem;
  metadata: TokenMetadata;
  inventory: InventoryInfo;
}

export function renderTokenCard(
  enriched: EnrichedToken,
  onBuy: (item: CatalogItem) => void,
): HTMLElement {
  const { item, metadata, inventory } = enriched;
  const card = document.createElement("article");
  card.className = "token-card" + (inventory.inStock ? "" : " is-disabled");

  const displayName = metadata.name || item.name;
  const monogram = monogramFromMint(item.mint);

  card.innerHTML = `
    <div class="token-card-media"></div>
    <div class="token-card-body">
      <h3>${escapeHtml(displayName)}</h3>
      <p class="token-card-sub">${escapeHtml(item.name)}</p>
      <p class="token-card-mint">${escapeHtml(item.mint)}</p>
      <p class="token-price">${formatUnitPriceLabel(item)}</p>
      <p class="token-stock ${inventory.inStock ? "in-stock" : "out-of-stock"}">
        ${inventory.inStock ? `In stock: ${escapeHtml(inventory.uiAmount)}` : "Out of stock"}
      </p>
      <button type="button" class="btn btn-primary token-card-buy" ${inventory.inStock ? "" : "disabled"}>
        Buy
      </button>
    </div>
  `;

  const media = card.querySelector(".token-card-media") as HTMLElement;
  mountTokenMedia(media, metadata.imageUrl, displayName, monogram);

  card.querySelector("button")?.addEventListener("click", () => onBuy(item));
  return card;
}

function mountTokenMedia(
  media: HTMLElement,
  imageUrl: string | null,
  displayName: string,
  monogram: string,
): void {
  if (imageUrl) {
    const img = document.createElement("img");
    img.src = imageUrl;
    img.alt = displayName;
    img.loading = "lazy";
    img.decoding = "async";
    img.className = "token-card-image";
    img.addEventListener("error", () => {
      media.replaceChildren(buildMonogram(monogram));
    });
    media.appendChild(img);
    return;
  }
  media.appendChild(buildMonogram(monogram));
}

function buildMonogram(monogram: string): HTMLElement {
  const el = document.createElement("div");
  el.className = "token-monogram";
  el.textContent = monogram;
  return el;
}

export function renderTokenShowcase(
  container: HTMLElement,
  catalog: CatalogDocument,
  enriched: EnrichedToken[],
  onBuy: (item: CatalogItem) => void,
): void {
  const grid = document.createElement("div");
  grid.className = "token-grid";
  const carousel = document.createElement("div");
  carousel.className = "token-carousel";

  for (const e of enriched) {
    const card = renderTokenCard(e, onBuy);
    grid.appendChild(card.cloneNode(true) as HTMLElement);
    carousel.appendChild(card);
  }

  // Re-bind buy on grid clones
  grid.querySelectorAll(".token-card").forEach((el, i) => {
    const btn = el.querySelector("button");
    btn?.addEventListener("click", () => onBuy(enriched[i].item));
  });

  container.replaceChildren(grid, carousel);
}

function formatUnitPriceLabel(item: CatalogItem): string {
  const price = escapeHtml(item.priceUsdcUi);
  const deliverN = Number(item.deliverAmountUi.trim());
  const singleToken = Number.isFinite(deliverN) && Math.abs(deliverN - 1) < 1e-9;

  if (singleToken) {
    return `<strong>${price} USDC</strong> per token`;
  }

  const deliver = escapeHtml(formatDeliverAmountUi(item.deliverAmountUi));
  return `<strong>${price} USDC</strong> per unit · ${deliver} tokens`;
}

function formatDeliverAmountUi(deliverUi: string): string {
  const trimmed = deliverUi.trim();
  const n = Number(trimmed);
  if (!Number.isFinite(n)) return trimmed;
  if (Number.isInteger(n)) return n.toLocaleString("en-US");
  return n.toLocaleString("en-US", { maximumFractionDigits: 18 }).replace(/\.?0+$/, "");
}

function escapeHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}
