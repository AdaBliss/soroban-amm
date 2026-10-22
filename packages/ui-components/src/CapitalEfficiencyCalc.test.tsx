import { useState } from "react";
import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { CapitalEfficiencyCalc } from "./CapitalEfficiencyCalc.js";
import type { PriceRange } from "./types.js";

/** Harness that keeps the controlled range in state, as a real app would. */
function EditableRangeHarness({ onRangeChange }: { onRangeChange: (r: PriceRange) => void }) {
  const [range, setRange] = useState<PriceRange>({ lower: 80, upper: 120 });
  return (
    <CapitalEfficiencyCalc
      currentPrice={100}
      priceRange={range}
      tokenA="XLM"
      tokenB="USDC"
      onRangeChange={(r) => {
        setRange(r);
        onRangeChange(r);
      }}
    />
  );
}

describe("CapitalEfficiencyCalc", () => {
  it("renders without crashing given valid props", () => {
    render(
      <CapitalEfficiencyCalc currentPrice={100} priceRange={{ lower: 80, upper: 120 }} />,
    );
    expect(screen.getByRole("heading", { name: "Capital Efficiency" })).toBeInTheDocument();
  });

  it("displays the capital-efficiency multiplier text", () => {
    render(
      <CapitalEfficiencyCalc currentPrice={90} priceRange={{ lower: 80, upper: 100 }} />,
    );
    expect(screen.getByRole("status")).toHaveTextContent("18.4x");
  });

  it("displays a ~1x multiplier and the inactive caption when out of range", () => {
    render(
      <CapitalEfficiencyCalc currentPrice={10} priceRange={{ lower: 80, upper: 120 }} />,
    );
    expect(screen.getByRole("status")).toHaveTextContent("1.0x");
    expect(
      screen.getByText(/Price is outside range — position earns no fees/),
    ).toBeInTheDocument();
  });

  it("displays a caption that the capital works harder when in range", () => {
    render(
      <CapitalEfficiencyCalc currentPrice={100} priceRange={{ lower: 80, upper: 120 }} />,
    );
    expect(screen.getByText(/works .*× harder than a full-range position/)).toBeInTheDocument();
  });

  it("formats the concentrated capital and full-range entries in the table", () => {
    render(
      <CapitalEfficiencyCalc
        currentPrice={90}
        priceRange={{ lower: 80, upper: 100 }}
        depositUsd={10_000}
      />,
    );
    expect(screen.getByText("Full range (v2-style)")).toBeInTheDocument();
    expect(screen.getByText("Concentrated (80.0000 – 100.0000)")).toBeInTheDocument();
    expect(screen.getByText("$542.54")).toBeInTheDocument();
  });

  it("renders range width, bounds and current price information", () => {
    render(
      <CapitalEfficiencyCalc currentPrice={100} priceRange={{ lower: 80, upper: 120 }} />,
    );
    expect(screen.getByText("Range width")).toBeInTheDocument();
    expect(screen.getByText(/40\.0%/)).toBeInTheDocument();
    expect(screen.getByText("80.000000")).toBeInTheDocument();
    expect(screen.getByText("120.000000")).toBeInTheDocument();
    expect(screen.getByText("100.000000")).toBeInTheDocument();
  });

  it("renders an editable range and emits range changes with the right payload", async () => {
    const user = userEvent.setup();
    const onRangeChange = vi.fn();
    render(<EditableRangeHarness onRangeChange={onRangeChange} />);
    const minInput = screen.getByLabelText("Minimum price for XLM/USDC");
    const maxInput = screen.getByLabelText("Maximum price for XLM/USDC");
    await user.clear(minInput);
    await user.type(minInput, "70");
    expect(onRangeChange).toHaveBeenLastCalledWith({ lower: 70, upper: 120 });

    await user.clear(maxInput);
    await user.type(maxInput, "150");
    expect(onRangeChange).toHaveBeenLastCalledWith({ lower: 70, upper: 150 });
    expect(minInput).toHaveValue(70);
    expect(maxInput).toHaveValue(150);
  });

  it("exposes an accessible live region and aria-label", () => {
    render(
      <CapitalEfficiencyCalc currentPrice={100} priceRange={{ lower: 80, upper: 120 }} />,
    );
    expect(screen.getByRole("status")).toBeInTheDocument();
    expect(screen.getByLabelText("Capital efficiency calculator")).toBeInTheDocument();
  });

  it("handles a zero deposit gracefully", () => {
    render(
      <CapitalEfficiencyCalc
        currentPrice={100}
        priceRange={{ lower: 80, upper: 120 }}
        depositUsd={0}
      />,
    );
    // Both the full-range and concentrated capital cells read $0.
    expect(screen.getAllByText("$0").length).toBeGreaterThanOrEqual(2);
  });

  it("does not render the editable range when onRangeChange is absent", () => {
    render(<CapitalEfficiencyCalc currentPrice={100} priceRange={{ lower: 80, upper: 120 }} />);
    expect(screen.queryByLabelText(/Minimum price for/)).not.toBeInTheDocument();
  });
});
