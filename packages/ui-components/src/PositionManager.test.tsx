import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { PositionManager } from "./PositionManager.js";
import type { PositionFormValues } from "./PositionManager.js";

async function setAmount(input: HTMLElement, value: string, user: ReturnType<typeof userEvent.setup>) {
  await user.tripleClick(input);
  await user.keyboard(value);
}

const baseProps = {
  currentPrice: 100,
  tokenA: "XLM",
  tokenB: "USDC",
};

describe("PositionManager", () => {
  it("renders a new position panel without crashing", () => {
    render(<PositionManager {...baseProps} />);
    expect(screen.getByRole("heading", { name: /New Position — XLM\/USDC/ })).toBeInTheDocument();
  });

  it("renders an edit position heading given a position id", () => {
    render(<PositionManager {...baseProps} position={{ id: "pos-1" }} />);
    expect(screen.getByRole("heading", { name: /Edit Position — XLM\/USDC/ })).toBeInTheDocument();
  });

  it("exposes a labelled form landmark and amount inputs", () => {
    render(<PositionManager {...baseProps} />);
    expect(screen.getByRole("form", { name: "Position manager" })).toBeInTheDocument();
    expect(screen.getByLabelText("XLM deposit amount")).toBeInTheDocument();
    expect(screen.getByLabelText("USDC deposit amount")).toBeInTheDocument();
  });

  it("shows the default 0% fee badge", () => {
    render(<PositionManager {...baseProps} />);
    expect(screen.getByText("0.3% fee")).toBeInTheDocument();
  });

  it("submits a valid position with the expected payload", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<(values: PositionFormValues) => void>();
    render(<PositionManager {...baseProps} onSubmit={onSubmit} />);
    await setAmount(screen.getByLabelText("XLM deposit amount"), "100", user);
    await setAmount(screen.getByLabelText("USDC deposit amount"), "200", user);
    await user.click(screen.getByRole("button", { name: "Add Liquidity" }));
    expect(onSubmit).toHaveBeenCalledTimes(1);
    const payload = onSubmit.mock.calls[0][0];
    expect(payload.amountA).toBe(100);
    expect(payload.amountB).toBe(200);
    expect(payload.feeBps).toBe(30);
    expect(payload.priceRange).toEqual({ lower: 80, upper: 120 });
  });

  it("rejects a negative amount and surfaces it to the user", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<(values: PositionFormValues) => void>();
    // Seed a negative amountA through the position prop (number inputs strip a
    // leading minus, so this drives the internal state directly).
    render(
      <PositionManager
        {...baseProps}
        onSubmit={onSubmit}
        position={{ amountA: -50, amountB: 100 }}
      />,
    );
    await user.click(screen.getByRole("button", { name: "Add Liquidity" }));
    expect(onSubmit).not.toHaveBeenCalled();
    expect(screen.getByRole("alert")).toHaveTextContent(/Amount A must be a positive number/);
  });

  it("rejects a zero amount and surfaces it to the user", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<(values: PositionFormValues) => void>();
    render(<PositionManager {...baseProps} onSubmit={onSubmit} />);
    await setAmount(screen.getByLabelText("XLM deposit amount"), "0", user);
    await setAmount(screen.getByLabelText("USDC deposit amount"), "50", user);
    await user.click(screen.getByRole("button", { name: "Add Liquidity" }));
    expect(onSubmit).not.toHaveBeenCalled();
    expect(screen.getByRole("alert")).toHaveTextContent(/Amount A/);
  });

  it("rejects a lower tick above the upper tick", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<(values: PositionFormValues) => void>();
    // Seed a malformed range via the position prop so the UI state carries it.
    render(
      <PositionManager
        {...baseProps}
        onSubmit={onSubmit}
        position={{ priceRange: { lower: 200, upper: 100 }, amountA: 100, amountB: 100 }}
      />,
    );
    await user.click(screen.getByRole("button", { name: "Add Liquidity" }));
    expect(onSubmit).not.toHaveBeenCalled();
    const alerts = screen.getAllByRole("alert");
    expect(
      alerts.some((a) => a.textContent?.includes("Lower price must be below the upper price")),
    ).toBe(true);
  });

  it("shows no error alert after a valid submit", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn<(values: PositionFormValues) => void>();
    render(<PositionManager {...baseProps} onSubmit={onSubmit} />);
    await setAmount(screen.getByLabelText("XLM deposit amount"), "100", user);
    await setAmount(screen.getByLabelText("USDC deposit amount"), "100", user);
    await user.click(screen.getByRole("button", { name: "Add Liquidity" }));
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("calls onCancel when the cancel button is pressed", async () => {
    const user = userEvent.setup();
    const onCancel = vi.fn();
    render(<PositionManager {...baseProps} onCancel={onCancel} />);
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onCancel).toHaveBeenCalled();
  });

  it("switches tab panels on tab click", async () => {
    const user = userEvent.setup();
    render(<PositionManager {...baseProps} />);
    const riskTab = screen.getByRole("tab", { name: /Risk/ });
    await user.click(riskTab);
    expect(riskTab).toHaveAttribute("aria-selected", "true");
    const panel = screen.getByRole("tabpanel");
    expect(panel).not.toHaveAttribute("hidden");
  });

  it("lets a user change the fee tier and reflect it in the badge", async () => {
    const user = userEvent.setup();
    render(<PositionManager {...baseProps} />);
    await user.click(screen.getByRole("tab", { name: "Fee Tier" }));
    await user.click(screen.getByRole("radio", { name: /Exotic — 1% fee/ }));
    expect(screen.getByText("1% fee")).toBeInTheDocument();
  });

  it("renders the risk tab with a warning emoji when risk is not low", () => {
    render(<PositionManager {...baseProps} poolTvl={500} priceDeviationBps={600} />);
    const riskTab = screen.getByRole("tab", { name: /Risk/ });
    expect(riskTab.textContent).toContain("⚠");
  });
});
