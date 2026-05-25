import { Connection, PublicKey } from "@solana/web3.js";

const TOKEN_METADATA_PROGRAM_ID = new PublicKey(
  "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s",
);

export interface TokenMetadata {
  name: string;
  symbol: string;
  imageUrl: string | null;
}

function metadataPda(mint: PublicKey): PublicKey {
  const [pda] = PublicKey.findProgramAddressSync(
    [
      Buffer.from("metadata"),
      TOKEN_METADATA_PROGRAM_ID.toBuffer(),
      mint.toBuffer(),
    ],
    TOKEN_METADATA_PROGRAM_ID,
  );
  return pda;
}

function readBorshString(
  data: Uint8Array,
  offset: number,
): { value: string; next: number } | null {
  if (offset + 4 > data.length) return null;
  const len = new DataView(data.buffer, data.byteOffset, data.byteLength).getUint32(
    offset,
    true,
  );
  const start = offset + 4;
  const end = start + len;
  if (end > data.length) return null;
  const value = new TextDecoder()
    .decode(data.slice(start, end))
    .replace(/\0/g, "")
    .trim();
  return { value, next: end };
}

function parseMetadataAccount(data: Uint8Array): { name: string; symbol: string; uri: string } | null {
  // Metadata account: key(1) + update_authority(32) + mint(32) + Data { name, symbol, uri } (Borsh strings)
  if (data.length < 65) return null;
  if (data[0] !== 4) return null;

  let offset = 1 + 32 + 32;
  const namePart = readBorshString(data, offset);
  if (!namePart) return null;
  const symbolPart = readBorshString(data, namePart.next);
  if (!symbolPart) return null;
  const uriPart = readBorshString(data, symbolPart.next);
  if (!uriPart) return null;

  return {
    name: namePart.value,
    symbol: symbolPart.value,
    uri: uriPart.value,
  };
}

function normalizeFetchableUri(uri: string): string | null {
  const trimmed = uri.trim();
  if (!trimmed) return null;
  if (trimmed.startsWith("http://") || trimmed.startsWith("https://")) {
    return trimmed;
  }
  if (trimmed.startsWith("ipfs://")) {
    return `https://gateway.pinata.cloud/ipfs/${trimmed.slice("ipfs://".length)}`;
  }
  return null;
}

async function fetchImageFromUri(uri: string): Promise<string | null> {
  const fetchUrl = normalizeFetchableUri(uri);
  if (!fetchUrl) return null;
  try {
    const controller = new AbortController();
    const t = setTimeout(() => controller.abort(), 8000);
    const res = await fetch(fetchUrl, { signal: controller.signal });
    clearTimeout(t);
    if (!res.ok) return null;
    const json = (await res.json()) as { image?: string };
    if (typeof json.image !== "string" || !json.image.trim()) return null;
    return normalizeFetchableUri(json.image) ?? json.image.trim();
  } catch {
    return null;
  }
}

export async function fetchTokenMetadata(
  connection: Connection,
  mintStr: string,
): Promise<TokenMetadata> {
  try {
    const mint = new PublicKey(mintStr);
    const pda = metadataPda(mint);
    const info = await connection.getAccountInfo(pda);
    if (!info?.data) {
      return { name: "", symbol: "", imageUrl: null };
    }
    const parsed = parseMetadataAccount(info.data);
    if (!parsed) {
      return { name: "", symbol: "", imageUrl: null };
    }
    const imageUrl = await fetchImageFromUri(parsed.uri);
    return {
      name: parsed.name || "",
      symbol: parsed.symbol || "",
      imageUrl,
    };
  } catch {
    return { name: "", symbol: "", imageUrl: null };
  }
}

export function monogramFromMint(mint: string): string {
  return mint.slice(0, 2).toUpperCase();
}
