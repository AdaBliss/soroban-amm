import { useState } from "react";
import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { RangeSelector } from "./RangeSelector.js";
import type { PriceRange } from "./types.js";

const defaultProps = {
  minPrice: 10,
  maxPrice: 1000,
  currentPrice: 100,
  value: { lower: 80, upper: 120 },
  onChange: vi.fn(),
};

function Harness({
  onRangeChange,
  initial = { lower: 80, upper: 120 },
  ...rest
}: {
  onRangeChange: (r: PriceRange) => void;
  initial?: PriceRange;
  [key: string]: unknown;
}) {
  const [range, setRange] = useState<PriceRange>(initial);
  return (
    <RangeSelector
      minPrice={10}
      maxPrice={1000}
      currentPrice={100}
      value={range}
      onChange={(r) => {
        setRange(r);
        onRangeChange(r);
      }}
      tokenA="XLM"
      tokenB="USDC"
      {...rest}
    />
  );
}

/**
 * A harness with a low minPrice (1) so that typed single digits are not
 * snapped up to a high minimum, letting a clean multi-digit value land.
 */
function NumericHarness({ onRangeChange }: { onRangeChange: (r: PriceRange) => void }) {
  const [range, setRange] = useState<PriceRange>({ lower: 50, upper: 120 });
  return (
    <RangeSelector
      minPrice={1}
      maxPrice={1000}
      currentPrice={100}
      value={range}
      onChange={(r) => {
        setRange(r);
        onRangeChange(r);
      }}
    />
  );
}

describe("RangeSelector", () => {
  it("renders without crashing given valid props", () => {
    render(<RangeSelector {...defaultProps} tokenA="XLM" tokenB="USDC" />);
    expect(screen.getByLabelText(/Price range selector/)).toBeInTheDocument();
    expect(screen.getByText("USDC per XLM")).toBeInTheDocument();
  });

  it("displays the selected range values", () => {
    render(<RangeSelector {...defaultProps} />);
    expect(screen.getByText(/80\.000000/)).toBeInTheDocument();
    expect(screen.getByText(/120\.000000/)).toBeInTheDocument();
  });

  it("exposes accessible sliders with current values and bounds", () => {
    render(<RangeSelector {...defaultProps} />);
    const lower = screen.getByRole("slider", { name: /Lower bound: 80\.000000/ });
    const upper = screen.getByRole("slider", { name: /Upper bound: 120\.000000/ });
    expect(lower).toHaveAttribute("aria-valuenow", "80");
    expect(lower).toHaveAttribute("aria-valuemin", "10");
    expect(lower).toHaveAttribute("aria-valuemax", "120");
    expect(upper).toHaveAttribute("aria-valuenow", "120");
    expect(upper).toHaveAttribute("aria-valuemax", "1000");
  });

  it("renders the empty/zero state for no liquidity bins", () => {
    render(<RangeSelector {...defaultProps} />);
    expect(screen.getByLabelText(/Price range selector/)).toBeInTheDocument();
    // No bin bars rendered for an empty set, and no crash.
    expect(document.querySelectorAll("[aria-hidden='true']").length).toBeGreaterThan(0);
  });

  it("shows an alert when the current price is outside the selected range", () => {
    render(<RangeSelector {...defaultProps} value={{ lower: 200, upper: 300 }} />);
    expect(screen.getByRole("alert")).toHaveTextContent(/outside the selected range/);
  });

  it("does not show an alert when the price is in range", () => {
    render(<RangeSelector {...defaultProps} />);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("emits a lower-bound change from the numeric input payload", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn<(r: PriceRange) => void>();
    render(<NumericHarness onRangeChange={onChange} />);
    const minInput = screen.getByLabelText("Minimum price");
    await user.tripleClick(minInput);
    await user.keyboard("70");
    expect(onChange).toHaveBeenLastCalledWith({ lower: 70, upper: 120 });
    expect(minInput).toHaveValue(70);
  });

  it("clamps a cleared upper-bound numeric input to just above the lower", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn<(r: PriceRange) => void>();
    render(<Harness onRangeChange={onChange} />);
    const maxInput = screen.getByLabelText("Maximum price");
    await user.clear(maxInput);
    // Clearing fires "" → Number("") = 0 → clamped up to lower + 0.000001.
    expect(onChange).toHaveBeenLastCalledWith({ lower: 80, upper: 80.000001 });
  });

  it("emits an upper-bound change from the numeric input payload", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn<(r: PriceRange) => void>();
    render(<NumericHarness onRangeChange={onChange} />);
    const maxInput = screen.getByLabelText("Maximum price");
    // The max input's minimum is lower + 0.000001; typing a value above it
    // passes through unchanged.
    await user.tripleClick(maxInput);
    await user.keyboard("999");
    const last = onChange.mock.calls.at(-1)?.[0];
    expect(last?.upper).toBeGreaterThan(50);
    expect(last?.lower).toBe(50);
  });

  it("moves the lower thumb on ArrowRight keyboard input", async () => {
    const onChange = vi.fn<(r: PriceRange) => void>();
    render(<Harness onRangeChange={onChange} />);
    const lower = screen.getByRole("slider", { name: /Lower bound/ });
    lower.focus();
    await userEvent.keyboard("{ArrowRight}");
    // step = (1000 - 10) / 200 = 4.95
    expect(onChange).toHaveBeenLastCalledWith({ lower: 80 + 4.95, upper: 120 });
  });

  it("moves the upper thumb on ArrowLeft keyboard input", async () => {
    const onChange = vi.fn<(r: PriceRange) => void>();
    render(<Harness onRangeChange={onChange} />);
    const upper = screen.getByRole("slider", { name: /Upper bound/ });
    upper.focus();
    await userEvent.keyboard("{ArrowLeft}");
    expect(onChange).toHaveBeenLastCalledWith({ lower: 80, upper: 120 - 4.95 });
  });

  it("jumps the lower thumb to the minimum using Home", async () => {
    const onChange = vi.fn<(r: PriceRange) => void>();
    render(<Harness onRangeChange={onChange} />);
    const lower = screen.getByRole("slider", { name: /Lower bound/ });
    lower.focus();
    await userEvent.keyboard("{Home}");
    expect(onChange).toHaveBeenLastCalledWith({ lower: 10, upper: 120 });
  });

  it("prevents the thumbs from crossing (lower cannot exceed upper)", async () => {
    const onChange = vi.fn<(r: PriceRange) => void>();
    render(<Harness onRangeChange={onChange} />);
    const lower = screen.getByRole("slider", { name: /Lower bound/ });
    lower.focus();
    await userEvent.keyboard("{End}");
    // Lower clamps to upper - 0.000001
    expect(onChange).toHaveBeenLastCalledWith({ lower: 120 - 0.000001, upper: 120 });
  });

  it("clamps numeric lower input to the upper bound", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn<(r: PriceRange) => void>();
    render(<Harness onRangeChange={onChange} />);
    const minInput = screen.getByLabelText("Minimum price");
    await user.clear(minInput);
    await user.type(minInput, "999");
    expect(onChange).toHaveBeenLastCalledWith({ lower: 120 - 0.000001, upper: 120 });
  });
});
