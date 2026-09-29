"""Tests for governance_client — proposal_status decoding and vote flow.

Covers:
- proposal_status normalises every shape scval.to_native can return for a
  unit-enum variant (list, dict, bare string) to a plain variant-name string.
- The vote step is attempted when status is "Active".
- The vote step is skipped when status is anything other than "Active".
- The script exits non-zero when the vote call raises.
- factory_client exits non-zero when create_pool raises.
- twap_client exits non-zero for unexpected failures, and uses EXIT_NO_SNAPSHOT
  (2) for the expected first-run "no snapshot yet" case.
"""
from __future__ import annotations

from unittest.mock import MagicMock, Mock, patch

import pytest

import factory_client
import governance_client
import twap_client
from common import decode_enum_variant


# ---------------------------------------------------------------------------
# decode_enum_variant — unit tests for all shapes scval.to_native returns
# ---------------------------------------------------------------------------

@pytest.mark.parametrize(
    "raw, expected",
    [
        # Unit variant decoded as a one-element list — the real shape on testnet
        (["Active"], "Active"),
        (["Passed"], "Passed"),
        (["Rejected"], "Rejected"),
        (["Executed"], "Executed"),
        (["Expired"], "Expired"),
        # Dict variant shape (tuple/struct variants)
        ({"Active": []}, "Active"),
        ({"Passed": [1, 2]}, "Passed"),
        # Bare string (future SDK versions may return this)
        ("Active", "Active"),
    ],
)
def test_decode_enum_variant_all_shapes(raw: object, expected: str) -> None:
    assert decode_enum_variant(raw) == expected


# ---------------------------------------------------------------------------
# proposal_status — uses decode_enum_variant under the hood
# ---------------------------------------------------------------------------

def test_proposal_status_returns_variant_name_for_list_shape() -> None:
    client = MagicMock()
    with patch("governance_client.simulate_contract_call", return_value=["Active"]):
        assert governance_client.proposal_status(client, 0) == "Active"


def test_proposal_status_returns_variant_name_for_dict_shape() -> None:
    client = MagicMock()
    with patch("governance_client.simulate_contract_call", return_value={"Active": []}):
        assert governance_client.proposal_status(client, 0) == "Active"


def test_proposal_status_returns_variant_name_for_bare_string() -> None:
    client = MagicMock()
    with patch("governance_client.simulate_contract_call", return_value="Active"):
        assert governance_client.proposal_status(client, 0) == "Active"


# ---------------------------------------------------------------------------
# governance_client.main — vote step runs when Active, skips otherwise
# ---------------------------------------------------------------------------

def _mock_governance_env(monkeypatch: pytest.MonkeyPatch, status_raw: object) -> None:
    monkeypatch.setenv("GOV_CONTRACT_ID", "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM")
    monkeypatch.setenv("SOURCE_SECRET", "SCZANGBA5YELMOUBOU65TXGT5ZYCNFGDL3IQPKNM5DCXMQXMFMK5K7W")
    monkeypatch.setattr(governance_client, "simulate_contract_call", Mock(return_value=status_raw))
    monkeypatch.setattr(governance_client, "submit_contract_call", Mock(return_value=0))
    # Patch ContractClient so no real network connection is made
    mock_client = MagicMock()
    monkeypatch.setattr(governance_client, "ContractClient", Mock(return_value=mock_client))


def test_vote_step_runs_when_status_is_active_list(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("PROPOSAL_ID", "1")
    monkeypatch.setenv("VOTE_CHOICE", "For")
    _mock_governance_env(monkeypatch, ["Active"])

    submit_mock = Mock(return_value=0)
    monkeypatch.setattr(governance_client, "submit_contract_call", submit_mock)

    result = governance_client.main()

    assert result == 0
    # submit_contract_call must have been called for the vote
    assert submit_mock.called
    call_args = submit_mock.call_args
    assert call_args.args[2] == "vote"


def test_vote_step_skipped_when_not_active(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("PROPOSAL_ID", "1")
    _mock_governance_env(monkeypatch, ["Passed"])

    submit_mock = Mock(return_value=0)
    monkeypatch.setattr(governance_client, "submit_contract_call", submit_mock)

    result = governance_client.main()

    assert result == 0
    # No vote submission should have been made
    for call in submit_mock.call_args_list:
        assert call.args[2] != "vote", "vote must not be submitted when status is not Active"


def test_vote_failure_returns_nonzero(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("PROPOSAL_ID", "1")
    monkeypatch.setenv("VOTE_CHOICE", "For")
    _mock_governance_env(monkeypatch, ["Active"])

    def raise_on_vote(client: object, kp: object, method: str, *args: object) -> object:
        if method == "vote":
            raise RuntimeError("vote rejected")
        return 0

    monkeypatch.setattr(governance_client, "submit_contract_call", raise_on_vote)

    result = governance_client.main()

    assert result != 0, "main() must return non-zero when vote raises"


# ---------------------------------------------------------------------------
# factory_client.main — exits non-zero when create_pool raises
# ---------------------------------------------------------------------------

def test_factory_create_pool_failure_returns_nonzero(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("FACTORY_CONTRACT_ID", "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM")
    monkeypatch.setenv("SOURCE_SECRET", "SCZANGBA5YELMOUBOU65TXGT5ZYCNFGDL3IQPKNM5DCXMQXMFMK5K7W")
    monkeypatch.setenv("TOKEN_A_CONTRACT_ID", "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM")
    monkeypatch.setenv("TOKEN_B_CONTRACT_ID", "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM")

    mock_client = MagicMock()
    monkeypatch.setattr(factory_client, "ContractClient", Mock(return_value=mock_client))
    # get_pool returns None so the create path is taken
    monkeypatch.setattr(factory_client, "simulate_contract_call", Mock(return_value=None))
    # create_pool raises
    monkeypatch.setattr(factory_client, "submit_contract_call", Mock(side_effect=RuntimeError("deploy failed")))

    result = factory_client.main()

    assert result != 0, "main() must return non-zero when create_pool raises"


# ---------------------------------------------------------------------------
# twap_client.main — exit codes for various failure modes
# ---------------------------------------------------------------------------

def _base_twap_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("TWAP_CONTRACT_ID", "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM")
    monkeypatch.setenv("POOL_CONTRACT_ID", "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM")
    monkeypatch.setenv("SOURCE_SECRET", "SCZANGBA5YELMOUBOU65TXGT5ZYCNFGDL3IQPKNM5DCXMQXMFMK5K7W")
    mock_client = MagicMock()
    monkeypatch.setattr(twap_client, "ContractClient", Mock(return_value=mock_client))


def test_twap_save_snapshot_failure_returns_1(monkeypatch: pytest.MonkeyPatch) -> None:
    _base_twap_env(monkeypatch)
    monkeypatch.setattr(twap_client, "submit_contract_call", Mock(side_effect=RuntimeError("rpc error")))

    result = twap_client.main()

    assert result == 1


def test_twap_no_snapshot_yet_returns_exit_no_snapshot(monkeypatch: pytest.MonkeyPatch) -> None:
    _base_twap_env(monkeypatch)
    monkeypatch.setattr(twap_client, "submit_contract_call", Mock(return_value=None))

    call_count = 0

    def simulate_side_effect(*args: object, **kwargs: object) -> object:
        nonlocal call_count
        call_count += 1
        # First simulate call is get_twap_price — raise to simulate no snapshot
        raise RuntimeError("no snapshot old enough")

    monkeypatch.setattr(twap_client, "simulate_contract_call", Mock(side_effect=simulate_side_effect))

    result = twap_client.main()

    assert result == twap_client.EXIT_NO_SNAPSHOT


def test_twap_get_twap_both_failure_returns_1(monkeypatch: pytest.MonkeyPatch) -> None:
    _base_twap_env(monkeypatch)
    monkeypatch.setattr(twap_client, "submit_contract_call", Mock(return_value=None))

    call_count = 0

    def simulate_side_effect(*args: object, **kwargs: object) -> object:
        nonlocal call_count
        call_count += 1
        if call_count == 1:
            return 1_000_000  # get_twap_price succeeds
        raise RuntimeError("get_twap_both failed")

    monkeypatch.setattr(twap_client, "simulate_contract_call", Mock(side_effect=simulate_side_effect))

    result = twap_client.main()

    assert result == 1


def test_twap_tracked_pools_failure_returns_1(monkeypatch: pytest.MonkeyPatch) -> None:
    _base_twap_env(monkeypatch)
    monkeypatch.setenv("SAVE_SNAPSHOT", "false")

    call_count = 0

    def simulate_side_effect(*args: object, **kwargs: object) -> object:
        nonlocal call_count
        call_count += 1
        if call_count == 1:
            return 1_000_000           # get_twap_price
        if call_count == 2:
            return [1_000_000, 999_000]  # get_twap_both
        raise RuntimeError("tracked pools failed")

    monkeypatch.setattr(twap_client, "simulate_contract_call", Mock(side_effect=simulate_side_effect))

    result = twap_client.main()

    assert result == 1
