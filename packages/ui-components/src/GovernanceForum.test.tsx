import { describe, it, expect, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { GovernanceForum } from "./GovernanceForum.js";
import type { Proposal } from "./types.js";
import type { GovernanceForumProps } from "./GovernanceForum.js";

const proposal: Proposal = {
  id: "GIP-001",
  title: "Raise the fee ceiling",
  category: "params",
  status: "active",
  summary: "A summary.",
  body: "Full body text.",
  forVotes: 100,
  againstVotes: 50,
  quorumPct: 10,
  created: "2026-01-01",
  ends: "2026-03-01",
  author: "0xabc",
};

const baseProps = {
  proposals: [proposal],
  delegates: [{ name: "Alice", address: "G…1234", votingPower: "1M", participationRate: "82%", bio: "Delegate bio" }],
  totalSupply: 184_000_000,
};

describe("GovernanceForum", () => {
  it("renders without crashing and lists the proposals", () => {
    render(<GovernanceForum {...baseProps} />);
    // The governance label appears on the root and the banner.
    expect(screen.getAllByLabelText(/Soroban AMM Governance/).length).toBeGreaterThan(0);
    expect(screen.getByLabelText("Proposal GIP-001: Raise the fee ceiling")).toBeInTheDocument();
    expect(screen.getByText("Raise the fee ceiling")).toBeInTheDocument();
  });

  it("shows read-only mode when no wallet is connected", () => {
    render(<GovernanceForum {...baseProps} />);
    expect(screen.getByText("Read-only mode")).toBeInTheDocument();
  });

  it("shows the connected wallet address when connected", () => {
    render(<GovernanceForum {...baseProps} walletConnected walletAddress="GABC123" />);
    expect(screen.getByText("GABC123")).toBeInTheDocument();
  });

  it("calls onConnectWallet from the connect button", async () => {
    const user = userEvent.setup();
    const onConnectWallet = vi.fn();
    render(<GovernanceForum {...baseProps} onConnectWallet={onConnectWallet} />);
    await user.click(screen.getByRole("button", { name: "Connect wallet" }));
    expect(onConnectWallet).toHaveBeenCalled();
  });

  it("casts a For vote from the list and calls onVote with the right payload", async () => {
    const user = userEvent.setup();
    const onVote = vi.fn();
    render(<GovernanceForum {...baseProps} onVote={onVote} />);
    await user.click(screen.getByRole("button", { name: "Vote for on GIP-001" }));
    expect(onVote).toHaveBeenCalledWith("GIP-001", "for");
    expect(screen.getByText("You voted: for")).toBeInTheDocument();
  });

  it("casts Against and Abstain votes with the correct choice", async () => {
    const user = userEvent.setup();
    const onVote = vi.fn();
    render(<GovernanceForum {...baseProps} onVote={onVote} />);
    await user.click(screen.getByRole("button", { name: "Vote against on GIP-001" }));
    expect(onVote).toHaveBeenLastCalledWith("GIP-001", "against");
    await user.click(screen.getByRole("button", { name: "Vote abstain on GIP-001" }));
    expect(onVote).toHaveBeenLastCalledWith("GIP-001", "abstain");
  });

  it("updates the proposal's displayed vote counts when voting", async () => {
    const user = userEvent.setup();
    render(<GovernanceForum {...baseProps} onVote={vi.fn()} />);
    const article = screen.getByLabelText("Proposal GIP-001: Raise the fee ceiling");
    // Initial: 100 for / 50 against → 67% for.
    expect(within(article).getByLabelText("Voting: 67% for, 33% against")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Vote for on GIP-001" }));
    // For votes jump by 50,000 → 50.1K.
    expect(screen.getByText("50.1K")).toBeInTheDocument();
  });

  it("surfaces a success toast after casting a vote", async () => {
    const user = userEvent.setup();
    render(<GovernanceForum {...baseProps} />);
    await user.click(screen.getByRole("button", { name: "Vote for on GIP-001" }));
    expect(screen.getByRole("alert")).toHaveTextContent(/Vote cast/);
  });

  it("opens a proposal detail modal and votes from it", async () => {
    const user = userEvent.setup();
    const onVote = vi.fn();
    render(<GovernanceForum {...baseProps} onVote={onVote} />);
    await user.click(screen.getByLabelText("Proposal GIP-001: Raise the fee ceiling"));
    const dialog = screen.getByRole("dialog");
    expect(dialog).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Vote for on GIP-001" }));
    expect(onVote).toHaveBeenCalledWith("GIP-001", "for");
  });

  it("switches to the delegation tab and delegates to a custom address", async () => {
    const user = userEvent.setup();
    const onDelegate = vi.fn();
    render(<GovernanceForum {...baseProps} onDelegate={onDelegate} />);
    await user.click(screen.getByRole("tab", { name: "Delegation" }));
    expect(screen.getByText("Vote Delegation")).toBeInTheDocument();
    const addr = screen.getByLabelText("Custom delegate address");
    await user.type(addr, "GABC1234");
    await user.click(screen.getByRole("button", { name: "Delegate voting power" }));
    expect(onDelegate).toHaveBeenCalledWith("GABC1234");
  });

  it("errors on delegation with an empty address", async () => {
    const user = userEvent.setup();
    const onDelegate = vi.fn();
    render(<GovernanceForum {...baseProps} onDelegate={onDelegate} />);
    await user.click(screen.getByRole("tab", { name: "Delegation" }));
    await user.click(screen.getByRole("button", { name: "Delegate voting power" }));
    expect(onDelegate).not.toHaveBeenCalled();
    expect(screen.getByRole("alert")).toHaveTextContent(/valid Stellar address/i);
  });

  it("delegates to a recommended delegate", async () => {
    const user = userEvent.setup();
    const onDelegate = vi.fn();
    render(<GovernanceForum {...baseProps} onDelegate={onDelegate} />);
    await user.click(screen.getByRole("tab", { name: "Delegation" }));
    await user.click(screen.getByRole("button", { name: "Delegate to Alice" }));
    expect(onDelegate).toHaveBeenCalled();
  });

  it("renders the voting power calculator tab with a computed value", async () => {
    const user = userEvent.setup();
    render(<GovernanceForum {...baseProps} />);
    await user.click(screen.getByRole("tab", { name: "Power Calc" }));
    expect(screen.getByText("Voting Power Calculator")).toBeInTheDocument();
    // Default: 10000 tokens, 30% locked, 1.5x → base 11500 → "11.5K" VP.
    expect(screen.getAllByText("11.5K").length).toBeGreaterThan(0);
  });

  it("recomputes voting power when the lock duration changes", async () => {
    const user = userEvent.setup();
    render(<GovernanceForum {...baseProps} />);
    await user.click(screen.getByRole("tab", { name: "Power Calc" }));
    await user.selectOptions(screen.getByLabelText("Lock duration"), "2");
    // 10000 tokens, 30% locked at 2x → base = 7000 + 3000*2 = 13000 → "13.0K".
    expect(screen.getAllByText("13.0K").length).toBeGreaterThan(0);
  });

  it("renders the analytics tab with a history table", async () => {
    const user = userEvent.setup();
    render(<GovernanceForum {...baseProps} />);
    await user.click(screen.getByRole("tab", { name: "Analytics" }));
    expect(screen.getByText("Proposal History & Analytics")).toBeInTheDocument();
    expect(screen.getByRole("table", { name: "Full proposal history" })).toBeInTheDocument();
    expect(screen.getByText("Raise the fee ceiling")).toBeInTheDocument();
  });

  it("filters proposals by status", async () => {
    const user = userEvent.setup();
    render(<GovernanceForum {...baseProps} />);
    await user.selectOptions(screen.getByLabelText("Filter by status"), "passed");
    expect(screen.getByText(/No proposals match the current filters/)).toBeInTheDocument();
  });
});

describe("GovernanceForum proposal wizard", () => {
  it("submits a new proposal through the wizard", async () => {
    const user = userEvent.setup();
    const onProposalSubmit = vi.fn<NonNullable<GovernanceForumProps["onProposalSubmit"]>>();
    render(<GovernanceForum {...baseProps} onProposalSubmit={onProposalSubmit} />);
    await user.click(screen.getByRole("tab", { name: "+ New Proposal" }));

    await user.type(screen.getByLabelText("Title *"), "New idea");
    await user.selectOptions(screen.getByLabelText("Category *"), "protocol");
    await user.type(screen.getByLabelText("Summary *"), "A summary");
    await user.click(screen.getByRole("button", { name: /Next/ }));

    // Step 1 — Parameters
    await user.click(screen.getByRole("button", { name: /Next/ }));
    // Step 2 — Actions
    await user.click(screen.getByRole("button", { name: /Next/ }));
    // Step 3 — Review
    await user.click(screen.getByRole("button", { name: /Submit Proposal/ }));

    expect(onProposalSubmit).toHaveBeenCalledTimes(1);
    const payload = onProposalSubmit.mock.calls[0][0];
    expect(payload.title).toBe("New idea");
    expect(payload.category).toBe("protocol");
    expect(payload.summary).toBe("A summary");
  });

  it("blocks advancing from step 1 when required fields are empty", async () => {
    const user = userEvent.setup();
    const onProposalSubmit = vi.fn<NonNullable<GovernanceForumProps["onProposalSubmit"]>>();
    render(<GovernanceForum {...baseProps} onProposalSubmit={onProposalSubmit} />);
    await user.click(screen.getByRole("tab", { name: "+ New Proposal" }));
    await user.click(screen.getByRole("button", { name: /Next/ }));
    // Title empty → error toast, still on step 0.
    expect(onProposalSubmit).not.toHaveBeenCalled();
    expect(screen.getByRole("alert")).toHaveTextContent(/Title is required/);
    expect(screen.getByLabelText("Title *")).toBeInTheDocument();
  });

  it("allows adding a proposal action in the wizard", async () => {
    const user = userEvent.setup();
    render(<GovernanceForum {...baseProps} />);
    await user.click(screen.getByRole("tab", { name: "+ New Proposal" }));
    await user.type(screen.getByLabelText("Title *"), "T");
    await user.selectOptions(screen.getByLabelText("Category *"), "protocol");
    await user.type(screen.getByLabelText("Summary *"), "S");
    await user.click(screen.getByRole("button", { name: /Next/ }));
    await user.click(screen.getByRole("button", { name: /Next/ }));
    await user.click(screen.getByRole("button", { name: /Add Action/ }));
    expect(screen.getByLabelText("Contract ID")).toBeInTheDocument();
  });
});
