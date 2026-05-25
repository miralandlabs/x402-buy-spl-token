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

function readFixedString(data: Uint8Array, offset: number, maxLen: number): string {
  const slice = data.slice(offset, offset + maxLen);
  const nul = slice.indexOf(0);
  const end = nul >= 0 ? nul : maxLen;
  return new TextDecoder().decode(slice.slice(0, end)).replace(/\0/g, "").trim();
}

function parseMetadataAccount(data: Uint8Array): { name: string; symbol: string; uri: string } | null {
  // MetadataV1 layout: key(1) + update_authority(32) + mint(32) + name(32) + symbol(10) + uri(200)
  if (data.length < 65 + 32 + 10 + 200) return null;
  const key = data[0];
  if (key !== 4) return null;
  const name = readFixedString(data, 65, 32);
  const symbol = readFixedString(data, 97, 10);
  const uri = readFixedString(data, 107, 200);
  return { name, symbol, uri };
}

async function fetchImageFromUri(uri: string): Promise<string | null> {
  if (!uri || !uri.startsWith("http")) return null;
  try {
    const controller = new AbortController();
    const t = setTimeout(() => controller.abort(), 8000);
    const res = await fetch(uri, { signal: controller.signal });
    clearTimeout(t);
    if (!res.ok) return null;
    const json = (await res.json()) as { image?: string };
    return typeof json.image === "string" ? json.image : null;
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
