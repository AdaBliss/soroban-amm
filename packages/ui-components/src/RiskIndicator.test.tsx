import { describe, it, expect } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { RiskIndicator } from "./RiskIndicator.js";
import { assessPositionRisk, riskLevelForScore, RISK_THRESHOLDS } from "./lib/riskAssessment.js";
import type { RiskAssessment } from "./types.js";

const assessment: RiskAssessment = {
  level: "high",
  score: 45,
  factors: [
    { name: "Out of range", description: "Price is outside range.", severity: "critical" },
    { name: "High price deviation", description: "Unusual volatility.", severity: "high", value: 600, threshold: 500 },
  ],
};

describe("RiskIndicator", () => {
  it("renders the risk level label and score text", () => {
    render(<RiskIndicator assessment={assessment} />);
    expect(screen.getByText("High Risk")).toBeInTheDocument();
    expect(screen.getByText(/Score: 45\/100/)).toBeInTheDocument();
  });

  it("renders the factor summary count", () => {
    render(<RiskIndicator assessment={assessment} />);
    expect(screen.getByText(/2 risk factors identified/)).toBeInTheDocument();
  });

  it("names singular factor count correctly", () => {
    render(
      <RiskIndicator
        assessment={{ ...assessment, factors: [assessment.factors[0]] }}
      />,
    );
    expect(screen.getByText(/1 risk factor identified/)).toBeInTheDocument();
  });

  it("renders the empty/zero state when there are no factors", () => {
    render(<RiskIndicator assessment={{ level: "low", score: 100, factors: [] }} />);
    expect(screen.getByText(/0 risk factors identified/)).toBeInTheDocument();
    // No expand button when there is nothing to expand.
    expect(screen.queryByRole("button", { name: /details/i })).not.toBeInTheDocument();
  });

  it("is collapsed by default and expands detail factors on toggle", async () => {
    const user = userEvent.setup();
    render(<RiskIndicator assessment={assessment} />);
    expect(screen.queryByText("Unusual volatility.")).not.toBeInTheDocument();
    const toggle = screen.getByRole("button", { name: /Show details/i });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    await user.click(toggle);
    expect(screen.getByRole("button", { name: /Hide details/i })).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText("Unusual volatility.")).toBeInTheDocument();
  });

  it("renders expanded by default when defaultExpanded is true", () => {
    render(<RiskIndicator assessment={assessment} defaultExpanded />);
    expect(screen.getByText("Unusual volatility.")).toBeInTheDocument();
  });

  it("renders severity badges with accessible names", () => {
    render(<RiskIndicator assessment={assessment} defaultExpanded />);
    expect(screen.getByLabelText("Severity: Critical Risk")).toBeInTheDocument();
    expect(screen.getByLabelText("Severity: High Risk")).toBeInTheDocument();
  });

  it("exposes an accessible overall risk live region", () => {
    render(<RiskIndicator assessment={assessment} />);
    expect(screen.getByRole("status")).toHaveAttribute(
      "aria-label",
      "Overall risk: High Risk, score 45 out of 100",
    );
  });

  it("honours an aria-label override on the root", () => {
    render(<RiskIndicator assessment={assessment} aria-label="My indicator" />);
    expect(screen.getByLabelText("My indicator")).toBeInTheDocument();
  });

  it("renders a factor bar only when value and threshold are defined", () => {
    render(<RiskIndicator assessment={assessment} defaultExpanded />);
    const list = screen.getByRole("list", { name: "Risk factors" });
    const items = within(list).getAllByRole("listitem");
    expect(items).toHaveLength(2);
    // Only one factor (deviation) has value+threshold → one bar rendered.
    const bar = list.querySelector("[aria-hidden='true']");
    expect(bar).toBeInTheDocument();
    expect(list.querySelectorAll("[aria-hidden='true']")).toHaveLength(1);
  });

  it("applies the level border colour to the root", () => {
    const { container } = render(<RiskIndicator assessment={assessment} />);
    expect(container.firstChild).toHaveStyle({ borderColor: "#f0883e" });
  });

  it("renders an explicit unknown state when metrics are missing or invalid", () => {
    const { container } = render(
      <RiskIndicator
        assessment={assessPositionRisk({
          priceDeviationBps: NaN,
          rangePct: NaN,
          tvl: NaN,
          isInRange: true,
        })}
      />,
    );
    expect(screen.getByText("Risk Unknown")).toBeInTheDocument();
    expect(
      screen.getByText(/Position metrics are missing or invalid/),
    ).toBeInTheDocument();
    expect(container.firstChild).toHaveStyle({ borderColor: "#8b949e" });
    // The unknown state must not be announced as a healthy score.
    expect(screen.getByRole("status")).toHaveAttribute(
      "aria-label",
      "Overall risk: Risk Unknown, position metrics missing or invalid",
    );
  });

  it("does not render the numeric score gauge in the unknown state", () => {
    render(
      <RiskIndicator assessment={{ level: "unknown", score: 0, factors: [] }} />,
    );
    expect(screen.queryByText(/Score:/)).not.toBeInTheDocument();
  });

  it("renders the level implied by a score on the RISK_THRESHOLDS boundary", () => {
    // A score exactly on the low/medium boundary must render as Low Risk; one
    // just below it as Medium Risk. This ties the rendered level to the shared
    // thresholds rather than a private copy of the numbers.
    const { unmount } = render(
      <RiskIndicator
        assessment={{
          level: riskLevelForScore(RISK_THRESHOLDS.low),
          score: RISK_THRESHOLDS.low,
          factors: [],
        }}
      />,
    );
    expect(screen.getByText("Low Risk")).toBeInTheDocument();
    unmount();

    render(
      <RiskIndicator
        assessment={{
          level: riskLevelForScore(RISK_THRESHOLDS.low - 1),
          score: RISK_THRESHOLDS.low - 1,
          factors: [],
        }}
      />,
    );
    expect(screen.getByText("Medium Risk")).toBeInTheDocument();
  });

  it("renders the critical level for a score below the high threshold", () => {
    render(
      <RiskIndicator
        assessment={{
          level: riskLevelForScore(RISK_THRESHOLDS.high - 1),
          score: RISK_THRESHOLDS.high - 1,
          factors: [],
        }}
      />,
    );
    expect(screen.getByText("Critical Risk")).toBeInTheDocument();
  });

  it("changes the rendered level when a RISK_THRESHOLDS value changes", () => {
    // The component must derive its level from the shared thresholds, not from
    // a private copy: raising `low` above 85 turns an 85 into Medium Risk.
    const thresholds = RISK_THRESHOLDS as unknown as {
      low: number;
      medium: number;
      high: number;
    };
    const original = thresholds.low;
    thresholds.low = 90;
    try {
      expect(riskLevelForScore(85)).toBe("medium");
      render(
        <RiskIndicator
          assessment={{ level: riskLevelForScore(85), score: 85, factors: [] }}
        />,
      );
      expect(screen.getByText("Medium Risk")).toBeInTheDocument();
      expect(screen.queryByText("Low Risk")).not.toBeInTheDocument();
    } finally {
      thresholds.low = original;
    }
  });
});
