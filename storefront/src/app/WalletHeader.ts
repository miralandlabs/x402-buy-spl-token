import type { CatalogDocument } from "../services/catalog";
import { clusterLabel } from "../services/catalog";

export interface WalletState {
  pubkey: string | null;
}

type Listener = (state: WalletState) => void;

const listeners = new Set<Listener>();
let state: WalletState = { pubkey: null };

export function getWalletState(): WalletState {
  return state;
}

export function subscribeWallet(fn: Listener): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

function emit() {
  for (const fn of listeners) fn({ ...state });
}

export function initWalletBridge(): void {
  window.addEventListener("miracle-pubkey", ((ev: CustomEvent) => {
    const detail = ev.detail as { pubkeyBase58?: string | null };
    state = { pubkey: detail.pubkeyBase58 ?? null };
    emit();
  }) as EventListener);
}

export function connectWallet(): void {
  if (typeof window.ShowWalletModal === "function") {
    window.ShowWalletModal();
  } else {
    alert("Wallet adapter still loading — try again in a moment.");
  }
}

export function disconnectWallet(): void {
  if (typeof window.MiracleWalletDisconnecter === "function") {
    window.MiracleWalletDisconnecter();
  }
  state = { pubkey: null };
  emit();
}

export function truncatePubkey(pk: string): string {
  return `${pk.slice(0, 4)}…${pk.slice(-4)}`;
}

export function renderWalletHeader(root: HTMLElement, catalog: CatalogDocument): void {
  const cluster = clusterLabel(catalog.cluster);
  const pillClass =
    catalog.cluster === "mainnet-beta" ? "cluster-pill is-mainnet" : "cluster-pill";

  root.innerHTML = `
    <header class="topbar">
      <div class="brand">
        <h1>x402 SPL Shop</h1>
        <p>Humans welcome · SLA-Escrow protected</p>
      </div>
      <div class="topbar-actions">
        <span class="${pillClass}">${cluster}</span>
        <span class="wallet-chip" id="wallet-chip"></span>
        <button type="button" class="btn btn-primary" id="wallet-btn">Connect wallet</button>
      </div>
    </header>
  `;

  const btn = root.querySelector("#wallet-btn") as HTMLButtonElement;
  const chip = root.querySelector("#wallet-chip") as HTMLSpanElement;

  const sync = () => {
    const { pubkey } = getWalletState();
    if (pubkey) {
      chip.textContent = truncatePubkey(pubkey);
      btn.textContent = "Disconnect";
      btn.classList.remove("btn-primary");
      btn.classList.add("btn-ghost");
    } else {
      chip.textContent = "";
      btn.textContent = "Connect wallet";
      btn.classList.add("btn-primary");
      btn.classList.remove("btn-ghost");
    }
  };

  subscribeWallet(sync);
  sync();

  btn.addEventListener("click", () => {
    if (getWalletState().pubkey) disconnectWallet();
    else connectWallet();
  });
}

declare global {
  interface Window {
    ShowWalletModal?: () => void;
    MiracleWalletDisconnecter?: () => void;
    MountWalletAdapter?: () => void;
    RemountStorefrontWallet?: () => void;
    __STOREFRONT_RPC__?: string;
    __STOREFRONT_CONNECTED_PUBKEY__?: string;
  }
}
