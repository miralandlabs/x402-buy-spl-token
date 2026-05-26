import React, { useMemo, useCallback, useEffect } from "react";
import ReactDOM from "react-dom/client";
import {
  ConnectionProvider,
  WalletProvider,
  useWallet,
} from "@solana/wallet-adapter-react";
import {
  BaseWalletMultiButton,
  WalletModalProvider,
} from "@solana/wallet-adapter-react-ui";
import { PhantomWalletAdapter } from "@solana/wallet-adapter-phantom";
import { SolflareWalletAdapter } from "@solana/wallet-adapter-solflare";
import { TrustWalletAdapter } from "@solana/wallet-adapter-trust";
import { BackpackWalletAdapter } from "@solana/wallet-adapter-backpack";
import {
  ComputeBudgetProgram,
  TransactionMessage,
  VersionedTransaction,
} from "@solana/web3.js";
import * as buffer from "buffer";

window.Buffer = buffer.Buffer;
require("@solana/wallet-adapter-react-ui/styles.css");
require("./styles.css");

const LABELS = {
  "change-wallet": "Change wallet",
  connecting: "Connecting…",
  "copy-address": "Copy address",
  copied: "Copied",
  disconnect: "Disconnect",
  "has-wallet": "Connect",
  "no-wallet": "Connect",
};

function rpcEndpoint() {
  if (typeof window !== "undefined" && window.__STOREFRONT_RPC__) {
    return window.__STOREFRONT_RPC__;
  }
  return "https://api.mainnet-beta.solana.com";
}

const Wallet = () => {
  const endpoint = rpcEndpoint();
  const wallets = useMemo(
    () => [
      new PhantomWalletAdapter(),
      new SolflareWalletAdapter(),
      new TrustWalletAdapter(),
      new BackpackWalletAdapter(),
    ],
    [],
  );
  return (
    <ConnectionProvider endpoint={endpoint} key={endpoint}>
      <WalletProvider wallets={wallets} autoConnect={true}>
        <WalletModalProvider>
          <div className="storefront-wallet-hidden-trigger" aria-hidden="true">
            <BaseWalletMultiButton labels={LABELS} />
          </div>
          <Dispatcher />
          <Disconnect />
          <SignTransaction />
        </WalletModalProvider>
      </WalletProvider>
    </ConnectionProvider>
  );
};

function isComputeBudgetInstruction(ix) {
  return ix.programId.equals(ComputeBudgetProgram.programId);
}

/** Match pr402 `TxBudget::FundPayment` — last-wins anchor after wallet sign. */
function anchorEscrowComputeBudget(tx) {
  const decompiled = TransactionMessage.decompile(tx.message);
  const core = decompiled.instructions.filter(
    (ix) => !isComputeBudgetInstruction(ix),
  );
  const instructions = [
    ...core,
    ComputeBudgetProgram.setComputeUnitLimit({ units: 80_000 }),
    ComputeBudgetProgram.setComputeUnitPrice({ microLamports: 100_000 }),
  ];
  const message = new TransactionMessage({
    payerKey: decompiled.payerKey,
    recentBlockhash: decompiled.recentBlockhash,
    instructions,
  }).compileToLegacyMessage();
  return new VersionedTransaction(message);
}

function MountWalletAdapter() {
  const container = document.getElementById("miracle-wallet-adapter");
  if (!container) return;
  const root = ReactDOM.createRoot(container);
  root.render(<Wallet />);
}

function ShowWalletModal() {
  const root = document.getElementById("miracle-wallet-adapter");
  if (!root) return;
  const btn = root.querySelector(".wallet-adapter-button-trigger");
  if (btn) btn.click();
}

function DisconnectWallet() {
  const container = document.getElementById("miracle-wallet-adapter");
  if (container) {
    container.innerHTML = "";
    setTimeout(() => window.MountWalletAdapter(), 100);
  }
}

function Dispatcher() {
  const { publicKey } = useWallet();
  useEffect(() => {
    const pubkeyBase58 = publicKey ? publicKey.toBase58() : null;
    window.__STOREFRONT_CONNECTED_PUBKEY__ = pubkeyBase58;
    window.dispatchEvent(
      new CustomEvent("miracle-pubkey", {
        bubbles: true,
        detail: {
          pubkey: publicKey ? Array.from(publicKey.toBytes()) : null,
          pubkeyBase58,
        },
      }),
    );
  }, [publicKey]);
  return null;
}

function Disconnect() {
  const { disconnect } = useWallet();
  const callback = useCallback(async () => {
    try {
      await disconnect();
    } catch (e) {
      console.warn(e);
    }
  }, [disconnect]);
  window.MiracleWalletDisconnecter = callback;
  return null;
}

function SignTransaction() {
  const { publicKey, signTransaction } = useWallet();
  const callback = useCallback(
    async (msg) => {
      const tx = anchorEscrowComputeBudget(
        VersionedTransaction.deserialize(Buffer.from(msg.b64, "base64")),
      );
      const signed = await signTransaction(tx);
      return Buffer.from(signed.serialize()).toString("base64");
    },
    [publicKey, signTransaction],
  );
  window.MiracleTxSigner = callback;
  return null;
}

window.MountWalletAdapter = MountWalletAdapter;
window.ShowWalletModal = ShowWalletModal;
window.MiracleWalletDisconnecter = DisconnectWallet;

/** Re-mount when storefront sets RPC after catalog load */
window.RemountStorefrontWallet = function () {
  const container = document.getElementById("miracle-wallet-adapter");
  if (container) {
    container.innerHTML = "";
    MountWalletAdapter();
  }
};
