import { randomHex32, computeSlaHash } from "./canonicalJson";
import type { CatalogDocument } from "./catalog";
import { clusterLabel } from "./catalog";

declare global {
  interface Window {
    MiracleTxSigner?: (msg: { b64: string }) => Promise<string | undefined>;
    BuildAndSignSlaEscrowPayment?: (ctxB64: string) => Promise<string>;
  }
}

export interface QuoteResult {
  paymentRequiredBody: Record<string, unknown>;
  acceptsLine: Record<string, unknown>;
  commitMaterial: Record<string, unknown>;
  usdcAmountRaw: string;
  resource: Record<string, unknown>;
}

export interface PurchaseResult {
  status: string;
  paymentUid?: string;
  slaHash?: string;
  transferSignature?: string;
  deliverySignature?: string;
  evidenceUrl?: string;
}

function formatUsdcRaw(raw: string): string {
  const n = BigInt(raw);
  const whole = n / 1_000_000n;
  const frac = n % 1_000_000n;
  if (frac === 0n) return whole.toString();
  return `${whole}.${frac.toString().padStart(6, "0").replace(/0+$/, "")}`;
}

export function formatUsdcAmount(raw: string): string {
  return `${formatUsdcRaw(raw)} USDC`;
}

export async function fetchQuote(
  catalog: CatalogDocument,
  params: {
    token: string;
    quantity: number;
    recipientOwner: string;
    buyerNonce: string;
  },
): Promise<QuoteResult> {
  const qs = new URLSearchParams({
    token: params.token,
    quantity: String(params.quantity),
    recipient_owner: params.recipientOwner,
    buyer_nonce: params.buyerNonce,
  });
  const url = `/api/v1/buy-spl-token?${qs}`;
  const res = await fetch(url);
  if (res.status !== 402) {
    const text = await res.text();
    throw new Error(`Expected 402 quote, got ${res.status}: ${text}`);
  }
  const body = (await res.json()) as Record<string, unknown>;
  const accepts = body.accepts as Record<string, unknown>[] | undefined;
  if (!accepts?.[0]) throw new Error("402 missing accepts[0]");
  const acceptsLine = accepts[0];
  const extra = acceptsLine.extra as Record<string, unknown> | undefined;
  const commitMaterial = extra?.commitMaterial as Record<string, unknown> | undefined;
  if (!commitMaterial) throw new Error("402 missing commitMaterial");
  return {
    paymentRequiredBody: body,
    acceptsLine,
    commitMaterial,
    usdcAmountRaw: String(acceptsLine.amount),
    resource: (body.resource as Record<string, unknown>) ?? {},
  };
}

export async function confirmSlaEscrowPayment(details: {
  usdcAmountRaw: string;
  cluster: string;
  deliverAmountRaw: string;
  tokenName: string;
}): Promise<boolean> {
  const networkLabel = clusterLabel(details.cluster);
  return new Promise((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "confirm-overlay";
    overlay.innerHTML = `
      <div class="confirm-dialog">
        <h4>Confirm SLA-Escrow payment</h4>
        <p>Pay <strong>${formatUsdcAmount(details.usdcAmountRaw)}</strong> into escrow for <strong>${details.tokenName}</strong>.</p>
        <p>Delivery: <strong>${details.deliverAmountRaw}</strong> tokens (raw units per seller quote).</p>
        <p>Network (Solana ${networkLabel}):</p>
        <div class="confirm-actions">
          <button type="button" class="btn btn-ghost" data-action="cancel">Cancel</button>
          <button type="button" class="btn btn-primary" data-action="confirm">Confirm &amp; sign</button>
        </div>
      </div>
    `;
    const close = (ok: boolean) => {
      overlay.remove();
      resolve(ok);
    };
    overlay.querySelector('[data-action="cancel"]')?.addEventListener("click", () => close(false));
    overlay.querySelector('[data-action="confirm"]')?.addEventListener("click", () => close(true));
    document.body.appendChild(overlay);
  });
}

export async function executePurchase(
  catalog: CatalogDocument,
  quote: QuoteResult,
  payerPubkey: string,
  buyerNonce: string,
  queryParams: URLSearchParams,
): Promise<PurchaseResult> {
  const commit = quote.commitMaterial;
  const paymentUid = randomHex32();
  const slaHash = await computeSlaHash({
    mint: String(commit.tokenMint),
    decimals: Number(commit.tokenDecimals),
    deliverAmountRaw: String(commit.deliverAmountRaw),
    recipientOwner: String(commit.recipientOwner),
    sellerPubkey: String(commit.sellerPubkey),
    buyerNonce,
    paymentUid,
    cluster: String(commit.cluster),
    profileId: String(commit.profileId),
    version: Number(commit.version),
  });

  const extra = quote.acceptsLine.extra as Record<string, unknown>;
  const oracleAuthorities = extra.oracleAuthorities as string[] | undefined;
  const oracleAuthority = oracleAuthorities?.[0];
  if (!oracleAuthority) throw new Error("No oracle authority in 402 extra");

  const buildUrl = `${catalog.facilitatorUrl.replace(/\/$/, "")}/build-sla-escrow-payment-tx`;
  const buildBody = {
    payer: payerPubkey,
    accepted: quote.acceptsLine,
    resource: quote.resource,
    slaHash,
    paymentUidHex: paymentUid,
    oracleAuthority,
    skipSourceBalanceCheck: false,
    facilitatorPaysTransactionFees: false,
  };

  const buildRes = await fetch(buildUrl, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(buildBody),
  });
  if (!buildRes.ok) {
    const text = await buildRes.text();
    throw new Error(`build-sla-escrow-payment-tx failed (${buildRes.status}): ${text}`);
  }
  const buildJson = (await buildRes.json()) as {
    transaction: string;
    verifyBodyTemplate: Record<string, unknown>;
  };

  if (!window.MiracleTxSigner) {
    throw new Error("Wallet not ready — connect your wallet first");
  }
  const signedB64 = await window.MiracleTxSigner({ b64: buildJson.transaction });
  if (!signedB64) throw new Error("Transaction signing cancelled");

  const verifyBody = structuredClone(buildJson.verifyBodyTemplate) as {
    paymentPayload?: { payload?: { transaction?: string } };
  };
  if (!verifyBody.paymentPayload?.payload) {
    throw new Error("verifyBodyTemplate missing paymentPayload.payload");
  }
  verifyBody.paymentPayload.payload.transaction = signedB64;

  const paymentSignature = JSON.stringify(verifyBody);
  const paidUrl = `/api/v1/buy-spl-token?${queryParams}`;
  const paidRes = await fetch(paidUrl, {
    headers: { "PAYMENT-SIGNATURE": paymentSignature },
  });
  const paidText = await paidRes.text();
  if (!paidRes.ok) {
    throw new Error(`Paid GET failed (${paidRes.status}): ${paidText}`);
  }
  return JSON.parse(paidText) as PurchaseResult;
}

export function paymentErrorMessage(err: unknown): string {
  const msg = err instanceof Error ? err.message : String(err);
  if (msg.includes("User rejected") || msg.includes("cancelled")) {
    return "You cancelled the wallet signature.";
  }
  if (msg.includes("payment_ttl_too_short")) {
    return "Payment timeout is too short for delivery — contact the seller operator.";
  }
  if (msg.includes("sla_hash_mismatch")) {
    return "SLA hash mismatch — refresh and try again.";
  }
  return msg;
}
