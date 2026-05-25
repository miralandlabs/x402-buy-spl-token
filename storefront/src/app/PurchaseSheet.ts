import type { CatalogDocument, CatalogItem } from "../services/catalog";
import { randomHex32 } from "../services/canonicalJson";
import {
  fetchQuote,
  confirmSlaEscrowPayment,
  executePurchase,
  formatUsdcAmount,
  paymentErrorMessage,
  type PurchaseResult,
} from "../services/slaEscrow";
import { getWalletState, connectWallet } from "./WalletHeader";
import { solscanTxUrl } from "../services/cluster";

type Step = "idle" | "quote" | "sign" | "deliver" | "done";

export function openPurchaseSheet(
  catalog: CatalogDocument,
  item: CatalogItem,
  onClose: () => void,
): void {
  let quantity = 1;
  let step: Step = "idle";
  let buyerNonce = randomHex32();
  let quoteUsdcRaw = "";
  let deliverRaw = "";
  let queryParams = new URLSearchParams();
  let result: PurchaseResult | null = null;
  let errorMsg = "";

  const overlay = document.createElement("div");
  overlay.className = "overlay";

  const render = () => {
    const wallet = getWalletState().pubkey;
    overlay.innerHTML = `
      <div class="sheet" role="dialog" aria-modal="true">
        <h3>Buy ${escapeHtml(item.name)}</h3>
        ${renderSteps(step)}
        <div class="qty-control">
          <span>Quantity</span>
          <button type="button" class="btn btn-ghost qty-minus">−</button>
          <input type="number" min="1" max="10000" value="${quantity}" class="qty-input" />
          <button type="button" class="btn btn-ghost qty-plus">+</button>
        </div>
        ${
          quoteUsdcRaw
            ? `<div class="sheet-row"><span>Session total</span><strong>${formatUsdcAmount(quoteUsdcRaw)}</strong></div>`
            : ""
        }
        ${errorMsg ? `<p class="sheet-error">${escapeHtml(errorMsg)}</p>` : ""}
        ${step === "done" && result ? renderSuccess(result, catalog.cluster) : ""}
        <div class="sheet-actions">
          <button type="button" class="btn btn-ghost" data-close>Close</button>
          ${
            step === "done"
              ? ""
              : `<button type="button" class="btn btn-primary" data-primary ${
                  step !== "idle" ? "disabled" : ""
                }">${step === "idle" ? "Get quote & pay" : "Processing…"}</button>`
          }
        </div>
      </div>
    `;

    overlay.querySelector("[data-close]")?.addEventListener("click", () => {
      overlay.remove();
      onClose();
    });

    const input = overlay.querySelector(".qty-input") as HTMLInputElement;
    input?.addEventListener("change", () => {
      quantity = Math.min(10000, Math.max(1, parseInt(input.value, 10) || 1));
      input.value = String(quantity);
      buyerNonce = randomHex32();
      quoteUsdcRaw = "";
    });

    overlay.querySelector(".qty-minus")?.addEventListener("click", () => {
      quantity = Math.max(1, quantity - 1);
      buyerNonce = randomHex32();
      quoteUsdcRaw = "";
      render();
    });
    overlay.querySelector(".qty-plus")?.addEventListener("click", () => {
      quantity = Math.min(10000, quantity + 1);
      buyerNonce = randomHex32();
      quoteUsdcRaw = "";
      render();
    });

    overlay.querySelector("[data-primary]")?.addEventListener("click", () => void runPurchase());

    async function runPurchase() {
      errorMsg = "";
      if (!wallet) {
        connectWallet();
        errorMsg = "Connect your wallet to continue.";
        render();
        return;
      }

      try {
        step = "quote";
        render();
        queryParams = new URLSearchParams({
          token: item.mint,
          quantity: String(quantity),
          recipient_owner: wallet,
          buyer_nonce: buyerNonce,
        });
        const quote = await fetchQuote(catalog, {
          token: item.mint,
          quantity,
          recipientOwner: wallet,
          buyerNonce,
        });
        quoteUsdcRaw = quote.usdcAmountRaw;
        deliverRaw = String(quote.commitMaterial.deliverAmountRaw);

        const ok = await confirmSlaEscrowPayment({
          usdcAmountRaw: quoteUsdcRaw,
          cluster: catalog.cluster,
          deliverAmountRaw: deliverRaw,
          tokenName: item.name,
        });
        if (!ok) {
          step = "idle";
          render();
          return;
        }

        step = "sign";
        render();
        result = await executePurchase(catalog, quote, wallet, buyerNonce, queryParams);
        step = "done";
        render();
      } catch (e) {
        errorMsg = paymentErrorMessage(e);
        step = "idle";
        render();
      }
    }
  };

  render();
  document.body.appendChild(overlay);
}

function renderSteps(step: Step): string {
  const steps = [
    { label: "Quote" },
    { label: "Sign payment" },
    { label: "Deliver" },
    { label: "Done" },
  ];

  const activeIndex =
    step === "idle"
      ? -1
      : step === "quote"
        ? 0
        : step === "sign"
          ? 1
          : step === "deliver"
            ? 2
            : steps.length;

  const items = steps
    .map((s, i) => {
      let state: "done" | "active" | "pending";
      if (step === "done" || i < activeIndex) {
        state = "done";
      } else if (i === activeIndex) {
        state = "active";
      } else {
        state = "pending";
      }

      const marker = state === "done" ? "✓" : String(i + 1);
      return `
        <li class="purchase-step is-${state}">
          <span class="purchase-step-marker">${marker}</span>
          <span class="purchase-step-label">${s.label}</span>
        </li>
      `;
    })
    .join("");

  return `<ol class="purchase-stepper" aria-label="Purchase progress">${items}</ol>`;
}

function renderSuccess(result: PurchaseResult, cluster: string): string {
  const links: string[] = [];
  if (result.transferSignature) {
    links.push(
      `<a href="${solscanTxUrl(result.transferSignature, cluster)}" target="_blank" rel="noopener">Transfer tx</a>`,
    );
  }
  if (result.deliverySignature) {
    links.push(
      `<a href="${solscanTxUrl(result.deliverySignature, cluster)}" target="_blank" rel="noopener">Delivery tx</a>`,
    );
  }
  return `
    <div class="success-view">
      <div class="check" aria-hidden="true">✓</div>
      <p class="success-status">${escapeHtml(formatStatus(result.status))}</p>
      <div class="tx-links">${links.join("\n")}</div>
    </div>
  `;
}

function formatStatus(status: string): string {
  if (!status) return "Completed";
  return status.charAt(0).toUpperCase() + status.slice(1);
}

function escapeHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}
