"use client";

import React, { useState, useEffect } from "react";
import { useForm } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import * as z from "zod";
import { useTranslations } from "next-intl";
import { Button } from "@/components/ui/Button";
import { Input } from "@/components/ui/Input";
import { useWallet } from "../../../hooks/useWallet";
import { useNetwork } from "@/app/providers/NetworkProvider";
import {
  ArrowRight,
  CheckCircle2,
  ExternalLink,
  Wallet,
  AlertCircle,
  Loader2,
} from "lucide-react";
import {
  TransactionBuilder,
  Contract,
  xdr,
  Address,
} from "@stellar/stellar-sdk";
import { getTokenActionBlockedReason } from "@/components/TokenStatusBanner";
import type { TokenInfo, WalletTokenState } from "@/lib/stellar";
import { toBaseUnits } from "@/lib/utils";

const buildTransferSchema = (decimals: number) =>
  z.object({
    to: z.string().regex(/^G[A-Z2-7]{55}$/, "Invalid Stellar address (must start with G)"),
    amount: z
      .string()
      .refine((val) => !isNaN(Number(val)) && Number(val) > 0, "Amount must be positive")
      .refine(
        (val) => {
          try {
            toBaseUnits(val, decimals);
            return true;
          } catch {
            return false;
          }
        },
        `Amount supports at most ${decimals} decimal place${decimals === 1 ? "" : "s"}`,
      ),
  });

type TransferData = z.infer<typeof transferSchema>;

function i128ToScVal(value: bigint): xdr.ScVal {
  const isNegative = value < BigInt(0);
  const absValue = isNegative ? -value : value;
  const lo = absValue & BigInt("0xFFFFFFFFFFFFFFFF");
  const hi = absValue >> BigInt(64);
  const i128Parts = new xdr.Int128Parts({
    lo: xdr.Uint64.fromString(lo.toString()),
    hi: xdr.Int64.fromString((isNegative ? -hi : hi).toString()),
  });
  return xdr.ScVal.scvI128(i128Parts);
}

function addressToScVal(address: string): xdr.ScVal {
  return new Address(address).toScVal();
}

interface TransferPanelProps {
  contractId: string;
  tokenSymbol: string;
  tokenDecimals: number;
  tokenInfo?: Pick<TokenInfo, "isPaused">;
  walletState?: WalletTokenState | null;
}

export function TransferPanel({
  contractId,
  tokenSymbol,
  tokenDecimals,
  tokenInfo,
  walletState,
}: TransferPanelProps) {
  const tTransfer = useTranslations("transfer");
  const tCommon = useTranslations("common");
  const blockedReason = tokenInfo ? getTokenActionBlockedReason(tokenInfo, walletState) : null;
  const { signTransaction, publicKey, connected } = useWallet();
  const { networkConfig } = useNetwork();

  const [loading, setLoading] = useState(false);
  const [checkingBalance, setCheckingBalance] = useState(false);
  const [userBalance, setUserBalance] = useState<string | null>(null);
  const [lastTxHash, setLastTxHash] = useState<string | null>(null);
  const [success, setSuccess] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const form = useForm<TransferData>({ resolver: zodResolver(buildTransferSchema(tokenDecimals)) });

  const fetchBalance = async () => {
    if (!publicKey || !connected) { setUserBalance(null); return; }
    setCheckingBalance(true);
    try {
      const rpc = new (await import("@stellar/stellar-sdk")).rpc.Server(networkConfig.rpcUrl);
      const contract = new Contract(contractId);
      const account = new (await import("@stellar/stellar-sdk")).Account("GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF", "0");
      const tx = new TransactionBuilder(account, { fee: "100", networkPassphrase: networkConfig.passphrase })
        .addOperation(contract.call("balance", addressToScVal(publicKey)))
        .setTimeout(30).build();
      const sim = await rpc.simulateTransaction(tx);
      if ((await import("@stellar/stellar-sdk")).rpc.Api.isSimulationSuccess(sim) && sim.result) {
        const balanceScVal = sim.result.retval;
        const parts = balanceScVal.i128();
        const hi = BigInt(parts.hi().toString());
        const lo = BigInt(parts.lo().toString());
        const rawBalance = (hi << BigInt(64)) + lo;
        const divisor = BigInt(10) ** BigInt(tokenDecimals);
        const whole = rawBalance / divisor;
        const frac = rawBalance % divisor;
        if (frac === BigInt(0)) setUserBalance(whole.toString());
        else {
          const fracStr = frac.toString().padStart(tokenDecimals, "0").replace(/0+$/, "");
          setUserBalance(`${whole}.${fracStr}`);
        }
      } else setUserBalance("0");
    } catch (err) {
      console.error("Failed to fetch balance:", err);
      setUserBalance(null);
    } finally { setCheckingBalance(false); }
  };

  useEffect(() => { fetchBalance(); }, [publicKey, connected, contractId, tokenDecimals, networkConfig]);

  const handleTransfer = async (data: TransferData) => {
    if (!publicKey || !connected) { setError(tTransfer("connectFirst")); return; }
    if (blockedReason) { setError(blockedReason); return; }
    if (userBalance && parseFloat(data.amount) > parseFloat(userBalance)) { setError(tTransfer("insufficientBalance")); return; }
    setLoading(true); setSuccess(false); setError(null); setLastTxHash(null);
    try {
      const StellarSdk = await import("@stellar/stellar-sdk");
      const rpc = new StellarSdk.rpc.Server(networkConfig.rpcUrl);
      let rawAmount: bigint;
      try {
        rawAmount = toBaseUnits(data.amount, tokenDecimals);
      } catch (err) {
        form.setError("amount", { type: "manual", message: err instanceof Error ? err.message : "Invalid amount" });
        return;
      }
      const account = await rpc.getAccount(publicKey);
      const contract = new Contract(contractId);
      const tx = new TransactionBuilder(account, { fee: StellarSdk.BASE_FEE, networkPassphrase: networkConfig.passphrase })
        .addOperation(contract.call("transfer", addressToScVal(publicKey), addressToScVal(data.to), i128ToScVal(rawAmount)))
        .setTimeout(30).build();
      const simulated = await rpc.simulateTransaction(tx);
      if (StellarSdk.rpc.Api.isSimulationError(simulated)) throw new Error(tTransfer("simulationFailed", { error: simulated.error }));
      if (!StellarSdk.rpc.Api.isSimulationSuccess(simulated)) throw new Error("Transaction simulation was not successful");
      const assembled = StellarSdk.rpc.assembleTransaction(tx, simulated);
      const preparedTx = assembled.build();
      const xdrEncoded = preparedTx.toXDR();
      const signedXdr = await signTransaction(xdrEncoded, { networkPassphrase: networkConfig.passphrase });
      const signedTx = TransactionBuilder.fromXDR(signedXdr, networkConfig.passphrase);
      const result = await rpc.sendTransaction(signedTx as import("@stellar/stellar-sdk").Transaction);
      if (result.status === "ERROR") throw new Error(`Transaction failed: ${result.errorResult?.toXDR("base64")}`);
      let getResponse = await rpc.getTransaction(result.hash);
      let attempts = 0;
      while (getResponse.status === "NOT_FOUND" && attempts < 30) {
        await new Promise((resolve) => setTimeout(resolve, 1000));
        getResponse = await rpc.getTransaction(result.hash);
        attempts++;
      }
      if (getResponse.status === "NOT_FOUND") throw new Error("Transaction not found after polling");
      if (getResponse.status === "FAILED") throw new Error(`Transaction failed: ${getResponse.resultXdr?.toXDR("base64")}`);
      setLastTxHash(result.hash); setSuccess(true); form.reset();
      await fetchBalance();
    } catch (err) {
      console.error("Transfer failed:", err);
      const errorMessage = err instanceof Error ? err.message : "Transfer failed";
      if (errorMessage.includes("User declined")) setError(tTransfer("transactionCancelled"));
      else if (errorMessage.includes("Insufficient balance")) setError(tTransfer("insufficientForTransfer"));
      else setError(errorMessage);
    } finally { setLoading(false); }
  };

  if (!connected || !publicKey) {
    return (
      <section className="mt-8 w-full max-w-4xl">
        <div className="glass-card p-8 text-center">
          <Wallet className="mx-auto h-12 w-12 text-gray-500 mb-4" />
          <h3 className="text-lg font-semibold text-white mb-2">{tTransfer("connectTitle")}</h3>
          <p className="text-sm text-gray-400">{tTransfer("connectDescription", { symbol: tokenSymbol })}</p>
        </div>
      </section>
    );
  }

  if (userBalance === "0" || userBalance === "0.0") {
    return (
      <section className="mt-8 w-full max-w-4xl">
        <div className="glass-card p-8 text-center">
          <AlertCircle className="mx-auto h-12 w-12 text-yellow-500 mb-4" />
          <h3 className="text-lg font-semibold text-white mb-2">{tTransfer("noBalanceTitle")}</h3>
          <p className="text-sm text-gray-400">{tTransfer("noBalanceDescription", { symbol: tokenSymbol })}</p>
        </div>
      </section>
    );
  }

  return (
    <section className="mt-8 w-full max-w-4xl animate-in fade-in slide-in-from-bottom-4 duration-700">
      <div className="flex items-center justify-between mb-6">
        <div className="flex items-center gap-3">
          <ArrowRight className="w-6 h-6 text-stellar-400" />
          <h2 className="text-2xl font-bold text-white tracking-tight">{tTransfer("title")}</h2>
        </div>
        {lastTxHash && (
          <a href={`https://stellar.expert/explorer/${networkConfig.network}/tx/${lastTxHash}`} target="_blank" rel="noopener noreferrer"
            className="flex items-center gap-1.5 text-xs text-stellar-400 hover:text-stellar-300 transition-colors bg-stellar-400/10 px-3 py-1.5 rounded-full border border-stellar-400/20">
            {tTransfer("lastTx", { hash: lastTxHash.slice(0, 8) })}<ExternalLink className="w-3 h-3" />
          </a>
        )}
      </div>

      <div className="glass-card p-6 hover:border-stellar-500/30 transition-all duration-300">
        <div className="mb-6 p-4 bg-stellar-500/10 rounded-lg border border-stellar-500/20">
          <div className="flex items-center justify-between">
            <span className="text-sm text-gray-400">{tTransfer("yourBalance")}</span>
            {checkingBalance ? (
              <Loader2 className="h-4 w-4 animate-spin text-stellar-400" />
            ) : (
              <span className="text-lg font-bold text-white">{userBalance || "..."} {tokenSymbol}</span>
            )}
          </div>
        </div>

        <form onSubmit={form.handleSubmit(handleTransfer)} className="space-y-4">
          <Input label={tTransfer("recipientLabel")} placeholder={tTransfer("recipientPlaceholder")} className="bg-white/5 border-white/10"
            {...form.register("to")} error={form.formState.errors.to?.message} disabled={loading} />
          <Input label={tTransfer("amountLabel", { symbol: tokenSymbol })} type="number" step="any"
            placeholder={tTransfer("amountPlaceholder")} className="bg-white/5 border-white/10"
            {...form.register("amount")} error={form.formState.errors.amount?.message} disabled={loading} />

          {blockedReason && !error && (
            <div className="flex items-start gap-2 p-3 bg-orange-500/10 border border-orange-500/20 rounded-lg">
              <AlertCircle className="h-4 w-4 text-orange-400 mt-0.5 shrink-0" /><p className="text-sm text-orange-300">{blockedReason}</p>
            </div>
          )}
          {error && (
            <div className="flex items-start gap-2 p-3 bg-red-500/10 border border-red-500/20 rounded-lg">
              <AlertCircle className="h-4 w-4 text-red-400 mt-0.5 shrink-0" /><p className="text-sm text-red-400">{error}</p>
            </div>
          )}
          {success && (
            <div className="flex items-start gap-2 p-3 bg-green-500/10 border border-green-500/20 rounded-lg">
              <CheckCircle2 className="h-4 w-4 text-green-400 mt-0.5 shrink-0" /><p className="text-sm text-green-400">{tTransfer("transferSuccessful")}</p>
            </div>
          )}

          <Button type="submit" className="w-full mt-4 shadow-lg shadow-stellar-500/20" isLoading={loading} disabled={loading || checkingBalance || !!blockedReason}>
            {success ? (
              <span className="flex items-center gap-2"><CheckCircle2 className="w-4 h-4" /> {tTransfer("transferComplete")}</span>
            ) : tTransfer("transferTokens")}
          </Button>
        </form>

        <p className="mt-4 text-xs text-gray-500 text-center">{tTransfer("networkNote", { network: networkConfig.network })}</p>
      </div>
    </section>
  );
}
