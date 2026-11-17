import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { ProviderUsage } from "../lib/types";
import { UsageDetails, creditText } from "./ProviderUsage";

const usage = (p: Partial<ProviderUsage>): ProviderUsage => ({
  budget: null,
  budget_error: null,
  budget_supported: false,
  tokens_today: [1200, 300],
  tokens_month: [5000, 900],
  bills_credits: false,
  credits_today: 0,
  credits_month: 0,
  ...p,
});

describe("UsageDetails", () => {
  it("shows the tokens a provider reported", () => {
    render(<UsageDetails providerId="p" usage={usage({})} loading={false} />);
    expect(screen.getByText("Tokens today")).toBeInTheDocument();
    expect(screen.getByText("Tokens this month")).toBeInTheDocument();
    expect(screen.queryByText("Credits today")).not.toBeInTheDocument();
  });

  it("shows credits for a provider that bills credits (Kiro), not zero tokens", () => {
    render(<UsageDetails providerId="p" usage={usage({ bills_credits: true, tokens_today: [0, 0], credits_today: 0.37, credits_month: 4.5 })} loading={false} />);
    expect(screen.getByText("Credits today").nextSibling).toHaveTextContent("0.37 credits");
    expect(screen.getByText("Credits this month").nextSibling).toHaveTextContent("4.5 credits");
    expect(screen.queryByText("Tokens today")).not.toBeInTheDocument();
  });

  it("writes credits with at most two decimals", () => {
    expect(creditText(0)).toBe("0 credits");
    expect(creditText(1.001)).toBe("1 credit");
    expect(creditText(0.125)).toBe("0.13 credits");
  });
});
