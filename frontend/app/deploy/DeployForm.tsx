"use client";

import React, { useState, useEffect } from "react";
import type { Resolver } from "react-hook-form";
import { useForm } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import * as z from "zod";
import { useRouter } from "next/navigation";
import { useTranslations } from "next-intl";
import { Button } from "@/components/ui/Button";
import { ProgressBar } from "@/components/ui/ProgressBar";
import { PreflightCheckDisplay } from "@/components/ui/PreflightCheck";
import { StepMetadata } from "./steps/StepMetadata";
import { StepSupply } from "./steps/StepSupply";
import { StepAdmin } from "./steps/StepAdmin";
import { StepReview } from "./steps/StepReview";
import { FeeEstimation } from "./components/FeeEstimation";
import { useTransactionSimulator } from "@/hooks/useTransactionSimulator";
import { useWallet } from "@/app/hooks/useWallet";
import { savePendingMetadata } from "./utils/metadata";
import { trackDeployment } from "@/lib/deployments";
import { ArrowLeft, ArrowRight, Rocket, Wallet } from "lucide-react";
import { useNetwork } from "@/app/providers/NetworkProvider";
import { useToast } from "@/app/providers/ToastProvider";
import { useDeployToken, type DeployTokenError } from "../hooks/useDeployToken";
import { toBaseUnits } from "@/lib/utils";

/**
 * Wraps `toBaseUnits` so that a `RangeError` (too many decimal places for the
 * selected token precision) is surfaced as a validation failure instead of an
 * uncaught exception. Returns `null` when the value cannot be represented.
 */
const tryToBaseUnits = (value: string, decimals: number): bigint | null => {
  try {
    return toBaseUnits(value, decimals);
  } catch {
    return null;
  }
};

/**
 * Preview-safe conversion used during render. Never throws: if the value
 * cannot be represented at the selected precision, the raw string is returned
 * so the user can see what they typed.
 */
const previewBaseUnits = (value: string | undefined, decimals: number): string => {
  if (value == null || value === "") return "";
  const parsed = tryToBaseUnits(value, decimals);
  return parsed === null ? value : parsed.toString();
};

const integerString = z
  .string()
  .min(1, "Initial supply is required")
  .regex(/^[0-9]+$/, "Initial supply must be a whole number")
  .refine((value) => {
    try {
      return BigInt(value) > 0n;
    } catch {
      return false;
    }
  }, { message: "Initial supply must be at least 1" })
  .max(38, "Initial supply is too large");

const optionalIntegerString = z.preprocess(
  (value) => {
    if (value === "" || value === null || value === undefined) return undefined;
    return typeof value === "string" ? value.trim() : value;
  },
  z
    .string()
    .regex(/^[0-9]+$/, "Max supply must be a whole number")
    .refine((value) => {
      try {
        return BigInt(value) > 0n;
      } catch {
        return false;
      }
    }, { message: "Max supply must be at least 1" })
    .max(38, "Max supply is too large")
    .optional(),
);

const deploySchema = z
  .object({
    name: z.string().min(1, "Token name is required").max(32, "Name too long"),
    symbol: z.string().min(1, "Symbol is required").max(12, "Symbol too long"),
    decimals: z.number().min(0).max(14),
    initialSupply: integerString,
    maxSupply: optionalIntegerString,
    adminAddress: z
      .string()
      .regex(/^[GC][A-Z2-7]{55}$/, "Invalid Stellar address or contract ID"),
    adminMode: z.enum(["wallet", "custom"]),
    complianceNodeAddress: z
      .string()
      .regex(/^C[A-Z2-7]{55}$/, "Invalid compliance node contract ID")
      .optional()
      .or(z.literal("")),
    // Authorization flags
    authorizationRequired: z.boolean(),
    authorizationRevocable: z.boolean(),
    // Optional metadata fields
    description: z.string().optional(),
    logoUrl: z.string().optional(),
    website: z.string().optional(),
    twitter: z.string().optional(),
    discord: z.string().optional(),
  })
  .refine(
    (data) =>
      data.maxSupply == null || BigInt(data.initialSupply) <= BigInt(data.maxSupply),
    {
      message: "Initial supply cannot exceed maximum supply",
      path: ["initialSupply"],
    },
  );

export type DeployFormData = z.infer<typeof deploySchema>;

export default function DeployForm() {
  const [currentStep, setCurrentStep] = useState(1);
  const [isDeploying, setIsDeploying] = useState(false);
  const [estimatedFee, setEstimatedFee] = useState<string | null>(null);
  const [feeEstimationLoading, setFeeEstimationLoading] = useState(false);
  const [feeEstimationError, setFeeEstimationError] = useState<string | null>(null);
  const [announcement, setAnnouncement] = useState("");
  const [preflightResult, setPreflightResult] = useState<{
    isLoading: boolean;
    success: boolean;
    errors: string[];
    warnings: string[];
  } | null>(null);
  const [cooldownRemainingMs, setCooldownRemainingMs] = useState(0);

  const router = useRouter();
  const { publicKey, connect } = useWallet();
  const { deployToken } = useDeployToken();
  const COOLDOWN_MS = 60_000;

  const simulator = useTransactionSimulator();
  const { networkConfig } = useNetwork();
  const toast = useToast();

  const {
    register,
    handleSubmit,
    control,
    trigger,
    formState: { errors, isValid },
    watch,
  } = useForm<DeployFormData>({
    resolver: zodResolver(deploySchema) as unknown as Resolver<DeployFormData>,
    mode: "onChange",
    defaultValues: {
      decimals: 7,
      initialSupply: "",
      maxSupply: undefined,
      name: "",
      symbol: "",
      adminAddress: publicKey ?? "",
      adminMode: publicKey ? "wallet" : "custom",
      complianceNodeAddress: "",
      authorizationRequired: false,
      authorizationRevocable: false,
      description: "",
      logoUrl: "",
      website: "",
      twitter: "",
      discord: "",
    },
  });
  const tCommon = useTranslations("common");
  const tDeploy = useTranslations("deploy");
  const steps = [
    tDeploy("steps.metadata"),
    tDeploy("steps.supply"),
    tDeploy("steps.admin"),
    tDeploy("steps.review"),
  ];

  const nextStep = async () => {
    let fieldsToValidate: (keyof DeployFormData)[] = [];
    if (currentStep === 1) fieldsToValidate = ["name", "symbol", "decimals"];
    if (currentStep === 2) fieldsToValidate = ["initialSupply", "maxSupply"];
    if (currentStep === 3) fieldsToValidate = ["adminMode", "adminAddress", "complianceNodeAddress"];

    const isStepValid = await trigger(fieldsToValidate);
    if (isStepValid) {
      const nextStepNum = Math.min(currentStep + 1, steps.length);
      setCurrentStep(nextStepNum);
      
      if (nextStepNum === 4) {
        estimateFee();
      }
    }
  };

  const scaleSupplyValue = (value: string, decimals: number) => {
    if (decimals === 0) return BigInt(value);
    return BigInt(value + "0".repeat(decimals));
  };

  const estimateFee = async () => {
    const formData = watch();
    if (!formData.adminAddress || !formData.name || !formData.symbol) return;

    setFeeEstimationLoading(true);
    setFeeEstimationError(null);
    setEstimatedFee(null);

    try {
      const result = await simulator.checkTokenDeployment(
        formData.adminAddress,
        formData.name,
        formData.symbol,
        formData.decimals,
        toBaseUnits(formData.initialSupply ?? 0, formData.decimals),
        formData.maxSupply != null
          ? toBaseUnits(formData.maxSupply, formData.decimals)
          : null,
        formData.authorizationRequired ?? false,
        formData.authorizationRevocable ?? false,
      );

      if (result.estimatedFee) {
        setEstimatedFee(result.estimatedFee);
      }

      if (!result.success) {
        setFeeEstimationError(result.errors[0] || "Fee estimation failed");
      }
    } catch (error) {
      setFeeEstimationError(
        error instanceof Error ? error.message : "Failed to estimate fee"
      );
    } finally {
      setFeeEstimationLoading(false);
    }
  };

  const prevStep = () => {
    setCurrentStep((prev) => Math.max(prev - 1, 1));
  };

  const onSubmit = async (data: DeployFormData) => {
    setIsDeploying(true);
    setAnnouncement("Deploying token transaction.");
    setPreflightResult({
      isLoading: true,
      success: false,
      errors: [],
      warnings: [],
    });

    try {
      // Deploy the token contract with the form data
      const result = await deployToken({
        name: data.name,
        symbol: data.symbol,
        decimals: data.decimals,
        initialSupply: data.initialSupply,
        maxSupply: data.maxSupply != null ? data.maxSupply : undefined,
        adminAddress: data.adminAddress,
        authorizationRequired: data.authorizationRequired ?? false,
        authorizationRevocable: data.authorizationRevocable ?? false,
        complianceNodeAddress: data.complianceNodeAddress || undefined,
      });

      setPreflightResult({
        isLoading: false,
        success: true,
        errors: [],
        warnings: [],
      });
      setAnnouncement(
        `Token deployed successfully. Transaction hash ${result.transactionHash}.`,
      );

      // Save metadata client-side
      try {
        savePendingMetadata(data.symbol, {
          description: data.description,
          logoUrl: data.logoUrl,
          website: data.website,
          twitter: data.twitter,
          discord: data.discord,
        });
      } catch {
        // Ignore metadata save errors
      }

      // Set client-side deploy cooldown (per-wallet)
      try {
        const key = `soropad:lastDeploy:${publicKey ?? "anonymous"}`;
        localStorage.setItem(key, Date.now().toString());

        // Track deployment for user dashboard
        if (publicKey) {
          trackDeployment(publicKey, {
            contractId: result.contractId,
            name: data.name,
            symbol: data.symbol,
            network: networkConfig.network,
          });
        }
      } catch {
        // Ignore tracking errors
      }

      toast.show({
        title: "Token deployed successfully",
        message: `Contract ID: ${result.contractId}`,
        variant: "success",
        duration: 8_000,
        txHash: result.transactionHash,
      });
      router.push(`/dashboard/${result.contractId}`);
    } catch (err) {
      // Handle deployment errors
      const error = err as DeployTokenError;
      const errorDetails: string[] = [];

      if (error.type === "validation") {
        errorDetails.push(error.message);
      } else if (error.type === "simulation") {
        errorDetails.push(`Simulation error: ${error.message}`);
      } else if (error.type === "wallet") {
        errorDetails.push(error.message);
      } else if (error.type === "broadcast") {
        errorDetails.push(`Broadcast error: ${error.message}`);
      } else if (error.type === "timeout") {
        errorDetails.push(error.message);
      } else if (error.message) {
        errorDetails.push(error.message);
      } else {
        errorDetails.push("Token deployment failed. Please try again.");
      }

      setPreflightResult({
        isLoading: false,
        success: false,
        errors: errorDetails,
        warnings: [],
      });
      setAnnouncement(
        `Deployment failed. ${errorDetails.join(" ") || "Please try again."}`,
      );

      console.error("Deployment error:", err);
    } finally {
      setIsDeploying(false);
    }
  };

  // Cooldown timer: read last deploy timestamp
  useEffect(() => {
    let mounted = true;
    const key = `soropad:lastDeploy:${publicKey ?? "anonymous"}`;

    const updateRemaining = () => {
      try {
        const last = Number(localStorage.getItem(key) || 0);
        const remaining = Math.max(0, last ? last + COOLDOWN_MS - Date.now() : 0);
        if (mounted) {
          setCooldownRemainingMs(remaining);
        }
      } catch {
        // ignore
      }
    };

    updateRemaining();
    const id = setInterval(updateRemaining, 1000);
    return () => {
      mounted = false;
      clearInterval(id);
    };
  }, [publicKey, COOLDOWN_MS]);

  const isCooldownActive = cooldownRemainingMs > 0;
  const cooldownSeconds = Math.ceil(cooldownRemainingMs / 1000);

  return (
    <div className="w-full max-w-xl mx-auto">
      <div className="mb-10">
        <ProgressBar current={currentStep} total={steps.length} />
      </div>

      <form
        onSubmit={handleSubmit(onSubmit)}
        className="glass-card p-8 min-h-[400px] flex flex-col"
      >
        <p className="sr-only" role="status" aria-live="polite" aria-atomic="true">
          {announcement}
        </p>
        {!publicKey && currentStep < 4 && (
          <div className="mb-6 flex items-center gap-3 px-4 py-3 rounded-lg bg-stellar-500/10 border border-stellar-500/20 text-sm">
            <Wallet className="w-4 h-4 text-stellar-400 shrink-0" />
            <span className="text-gray-300 flex-1">Connect your wallet before deploying — you can still fill in the form now.</span>
            <button
              type="button"
              onClick={connect}
              className="text-stellar-400 hover:text-stellar-300 font-semibold text-xs whitespace-nowrap underline"
            >
              Connect
            </button>
          </div>
        )}

        <div className="grow">
          {currentStep === 1 && (
            <StepMetadata register={register} errors={errors} />
          )}
          {currentStep === 2 && (
            <StepSupply control={control} errors={errors} />
          )}
          {currentStep === 3 && (
            <StepAdmin
              register={register}
              errors={errors}
              control={control}
            />
          )}
          {currentStep === 4 && (
            <StepReview
              control={control}
              estimatedFee={estimatedFee}
              feeEstimationLoading={feeEstimationLoading}
              feeEstimationError={feeEstimationError}
            />
          )}
        </div>

        {/* Fee Estimation (shown on review step) */}
        {currentStep === 4 && (
          <div className="mt-6">
            <FeeEstimation
              estimatedFee={estimatedFee}
              isLoading={feeEstimationLoading}
              error={feeEstimationError}
            />
          </div>
        )}

        {/* Pre-flight check results (shown on review step) */}
        {currentStep === 4 && preflightResult && (
          <div className="mt-6 mb-6">
            <PreflightCheckDisplay
              isLoading={preflightResult.isLoading}
              errors={preflightResult.errors}
              warnings={preflightResult.warnings}
              successMessage={
                preflightResult.success
                  ? tDeploy("review.readyMessage")
                  : undefined
              }
              onDismiss={() => setPreflightResult(null)}
            />
          </div>
        )}

        <div className="mt-10 flex justify-between items-center bg-void-900/50 -mx-8 -mb-8 p-6 rounded-b-2xl border-t border-white/5">
          <Button
            type="button"
            variant="secondary"
            onClick={prevStep}
            disabled={
              currentStep === 1 ||
              isDeploying ||
              (preflightResult?.isLoading ?? false)
            }
            className="px-4 py-2 flex"
          >
            <ArrowLeft className="w-4 h-4" />
            <p>{tCommon("back")}</p>
          </Button>

          {currentStep < steps.length ? (
            <Button type="button" onClick={nextStep} className="px-6 py-2 flex">
              <p>{tCommon("continue")}</p>
              <ArrowRight className="w-4 h-4" />
            </Button>
          ) : (
            <div className="flex gap-3">
              <Button
                type="button"
                variant="secondary"
                onClick={async () => {
                  const formData = watch();
                  setPreflightResult({
                    isLoading: true,
                    success: false,
                    errors: [],
                    warnings: [],
                  });
                  try {
                    const result = await simulator.checkTokenDeployment(
                      formData.adminAddress,
                      formData.name,
                      formData.symbol,
                      formData.decimals,
                      toBaseUnits(formData.initialSupply ?? 0, formData.decimals),
                      formData.maxSupply != null
                        ? toBaseUnits(formData.maxSupply, formData.decimals)
                        : null,
                      formData.authorizationRequired ?? false,
                      formData.authorizationRevocable ?? false,
                      formData.complianceNodeAddress || null,
                      publicKey ?? undefined,
                    );
                    setPreflightResult({
                      isLoading: false,
                      success: result.success,
                      errors: result.errors,
                      warnings: result.warnings,
                    });
                    if (result.estimatedFee) {
                      setEstimatedFee(result.estimatedFee);
                    }
                  } catch (error) {
                    const errorMessage =
                      error instanceof Error ? error.message : "Unknown error";
                    setPreflightResult({
                      isLoading: false,
                      success: false,
                      errors: [errorMessage],
                      warnings: [],
                    });
                  }
                }}
                disabled={
                  !isValid ||
                  isDeploying ||
                  (preflightResult?.isLoading ?? false)
                }
                className="px-6 py-2"
              >
                {tCommon("check")}
              </Button>
              <Button
                type="submit"
                disabled={
                  !isValid ||
                  isDeploying ||
                  !(preflightResult?.success ?? false) ||
                  isCooldownActive
                }
                isLoading={isDeploying}
                className="px-8 py-2"
              >
                <Rocket className="w-4 h-4" />
                {isCooldownActive
                  ? `Cooldown: ${cooldownSeconds}s`
                  : tDeploy("buttons.deployToken")}
              </Button>
            </div>
          )}
        </div>
      </form>

      <div className="mt-8 text-center">
        <p className="text-xs text-gray-500">
          {tDeploy("stepIndicator", {
            current: currentStep,
            name: steps[currentStep - 1],
          })}
        </p>
      </div>
    </div>
  );
}
