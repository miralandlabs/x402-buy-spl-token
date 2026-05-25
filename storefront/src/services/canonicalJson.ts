/** Canonical JSON + SHA-256 for TransferSla (matches seller E2E script). */

export async function sha256Hex(bytes: Uint8Array): Promise<string> {
  const hash = await crypto.subtle.digest("SHA-256", bytes);
  return Array.from(new Uint8Array(hash))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

export function canonicalJsonStringify(value: unknown): string {
  return JSON.stringify(sortKeys(value));
}

function sortKeys(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map(sortKeys);
  }
  if (value !== null && typeof value === "object") {
    const obj = value as Record<string, unknown>;
    const sorted: Record<string, unknown> = {};
    for (const key of Object.keys(obj).sort()) {
      sorted[key] = sortKeys(obj[key]);
    }
    return sorted;
  }
  return value;
}

export interface SlaBuildInput {
  mint: string;
  decimals: number;
  deliverAmountRaw: string;
  recipientOwner: string;
  sellerPubkey: string;
  buyerNonce: string;
  paymentUid: string;
  cluster: string;
  profileId: string;
  version: number;
}

export async function computeSlaHash(input: SlaBuildInput): Promise<string> {
  const sla = {
    buyer_nonce: input.buyerNonce,
    cluster: input.cluster,
    expected_transfers: [
      {
        decimals: input.decimals,
        direction: "in",
        min_amount: input.deliverAmountRaw,
        mint: input.mint,
        recipient_owner: input.recipientOwner,
        sender_owner: input.sellerPubkey,
      },
    ],
    payment_uid: input.paymentUid,
    profile_id: input.profileId,
    version: input.version,
  };
  const canonical = canonicalJsonStringify(sla);
  const bytes = new TextEncoder().encode(canonical);
  return sha256Hex(bytes);
}

export function randomHex32(): string {
  const buf = new Uint8Array(32);
  crypto.getRandomValues(buf);
  return Array.from(buf)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}
