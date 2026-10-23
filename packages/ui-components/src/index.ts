export { RangeSelector } from "./RangeSelector.js";
export type { RangeSelectorProps, LiquidityBin } from "./RangeSelector.js";

export { FeeTierComparison } from "./FeeTierComparison.js";
export type { FeeTierComparisonProps } from "./FeeTierComparison.js";

export { CapitalEfficiencyCalc } from "./CapitalEfficiencyCalc.js";
export type { CapitalEfficiencyCalcProps } from "./CapitalEfficiencyCalc.js";
export {
  computeEfficiency,
  computeEfficiencyFromTicks,
  formatEfficiency,
  isInRange,
  rangePct,
} from "./lib/capitalEfficiency.js";

export { RiskIndicator } from "./RiskIndicator.js";
export type { RiskIndicatorProps } from "./RiskIndicator.js";

// The risk assessment lives in the lib and is the single implementation; it is
// re-exported here under the same public name the components have always used.
export {
  assessPositionRisk,
  riskLevelForScore,
  RISK_THRESHOLDS,
  INVALID_DATA_FACTOR,
} from "./lib/riskAssessment.js";
export type { RiskParams } from "./lib/riskAssessment.js";

export { validatePosition } from "./lib/positionValidation.js";
export type { PositionValidationInput } from "./lib/positionValidation.js";

export { PositionManager } from "./PositionManager.js";
export type { PositionManagerProps, PositionFormValues } from "./PositionManager.js";

export type {
  FeeTier,
  PriceRange,
  Position,
  RiskLevel,
  RiskAssessment,
  RiskFactor,
  BaseProps,
  // Governance
  ProposalStatus,
  ProposalCategory,
  VoteChoice,
  OnChainAction,
  Proposal,
  Delegate,
} from "./types.js";

export { GovernanceForum } from "./GovernanceForum.js";
export type { GovernanceForumProps } from "./GovernanceForum.js";

