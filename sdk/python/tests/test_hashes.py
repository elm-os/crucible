"""The hashes and prefixes the middleware sends instead of raw values (contracts 9.1, 9.4 and 9.6)."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from mcp_types import ClientCapabilities, ElicitationCapability

from crucible_sdk.mcp._middleware import capabilities_hash, ip_prefix, mrtr_hash, session_hash

# The repository root; `.parent` stops at the filesystem root when the SDK is checked out on its own.
REGISTRY = Path(__file__).resolve().parent.parent.parent.parent / "conformance" / "registry.json"
KEY = "csk_conformance00000000000000000000000000000000"

# The vectors of the conformance registry (`hashes.session_id` and `hashes.mrtr_state`).
SESSION_VECTORS = [
    ("3f8e1c2a-5b7d-4e9f-a1c3-d5e7f9a1b3c5", "84757436d5e0791bd4a8eaa6f1d05986"),
    ("conformance-session", "b1427bda7462603423a9e4a7b73fbdd0"),
]
MRTR_VECTORS = [
    ("crucible-conformance-request-state-1", "59c50f1c773bc1354d420d974c06c5af"),
    ("eyJzdGVwIjoiY29uZmlybWVkIn0.c2ln", "bec039d76aab792bd526d369321fb0c0"),
]


@pytest.mark.parametrize(("raw", "expected"), SESSION_VECTORS)
def test_session_hash(raw: str, expected: str) -> None:
    assert session_hash(raw) == expected


@pytest.mark.parametrize(("state", "expected"), MRTR_VECTORS)
def test_mrtr_hash(state: str, expected: str) -> None:
    assert mrtr_hash(KEY, state) == expected


@pytest.mark.skipif(not REGISTRY.exists(), reason="conformance/registry.json has not landed yet")
def test_the_registry_vectors() -> None:
    hashes = json.loads(REGISTRY.read_text("utf-8"))["hashes"]
    for vector in hashes["session_id"]["vectors"]:
        assert session_hash(vector["input"]) == vector["output"]
    for vector in hashes["mrtr_state"]["vectors"]:
        assert mrtr_hash(vector["key"], vector["input"]) == vector["output"]


@pytest.mark.parametrize(
    ("host", "prefix"),
    [
        ("203.0.113.42", "203.0.113.0/24"),
        ("127.0.0.1", "127.0.0.0/24"),
        ("2001:db8:1234:5678::1", "2001:db8:1234::/48"),
        ("::ffff:198.51.100.7", "198.51.100.0/24"),
        ("localhost", None),
        ("", None),
    ],
)
def test_ip_prefix(host: str, prefix: str | None) -> None:
    assert ip_prefix(host) == prefix


def test_capabilities_hash_is_stable_and_fits_an_otlp_int() -> None:
    model = ClientCapabilities(elicitation=ElicitationCapability())
    dumped = model.model_dump(mode="json", by_alias=True, exclude_none=True)
    assert capabilities_hash(model) == capabilities_hash(dumped)
    assert capabilities_hash({"a": 1, "b": {"c": 2}}) == capabilities_hash({"b": {"c": 2}, "a": 1})
    assert capabilities_hash({}) != capabilities_hash(dumped)
    assert 0 <= capabilities_hash(dumped) < 2**63
