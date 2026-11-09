"""Parity test: render the REAL Jinja LOGIC_BODY in HA and assert it matches decide.py.

Skipped automatically when HA is unreachable (so the offline unit tests still run in CI).
Set HA_URL / HA_TOKEN via ~/.config/ha/env. Renders the shared LOGIC_BODY (byte-identical
to what is deployed) with literal test inputs through HA's /api/template.
"""

from __future__ import annotations

import json
import os
import random
import urllib.request

import pytest

from dispatcher.decide import Inputs, decide_setpoint
from dispatcher.live_template import scenario_template

MODES = ["Uit", "Zon only", "6A — van net", "6A — van batterij", "Max onder piek", "Batterij → auto"]


def _load_env() -> dict[str, str] | None:
    """HA credentials from ~/.config/ha/env, with os.environ taking precedence.

    The override matters because this test renders through HA's own /api/template, so it silently
    SKIPS whenever HA is unreachable — and a skip reads as a pass in the summary. On 2026-08-30 the
    LAN path to HA dropped and 61 parity cases skipped while still exiting 0. Being able to point
    HA_URL at an SSH tunnel keeps the mirror actually verified from anywhere:

        HA_URL=http://localhost:18123 uv run pytest tests/test_parity.py
    """
    env: dict[str, str] = {}
    path = os.path.expanduser("~/.config/ha/env")
    if os.path.exists(path):
        for line in open(path):
            if "=" in line:
                k, v = line.strip().split("=", 1)
                env[k] = v
    for k in ("HA_URL", "HA_TOKEN"):
        if os.environ.get(k):
            env[k] = os.environ[k]
    return env if "HA_URL" in env and "HA_TOKEN" in env else None


def _render(env: dict[str, str], template: str) -> str | None:
    req = urllib.request.Request(
        env["HA_URL"].rstrip("/") + "/api/template",
        data=json.dumps({"template": template}).encode(),
        headers={"Authorization": "Bearer " + env["HA_TOKEN"], "Content-Type": "application/json"},
        method="POST",
    )
    try:
        return urllib.request.urlopen(req, timeout=10).read().decode()
    except Exception:
        return None


ENV = _load_env()


@pytest.mark.skipif(ENV is None, reason="HA env not configured")
def test_ha_reachable_and_parity_smoke():
    rendered = _render(ENV, scenario_template(_rand_inputs(random.Random(0))))
    if rendered is None:
        pytest.skip("HA unreachable")
    assert rendered.lstrip("-").isdigit()


def _rand_inputs(rng: random.Random) -> Inputs:
    return Inputs(
        battery_power_net=rng.choice([-3000, -1000, 0, 1000, 3000]),
        p1=rng.randint(-5000, 5000),
        soc=rng.choice([10, 15, 30, 40, 41, 55, 90, 96, 97, 99, 100]),
        is_ochtend=rng.random() < 0.3,
        ev_laadmodus=rng.choice(MODES),
        ev_higher_hungry=rng.random() < 0.5,
        ev_grid_laden=rng.random() < 0.3,
        battery_bridges=rng.random() < 0.5,
        ev_batterij_auto_laden=rng.random() < 0.3,
        inputs_valid=rng.random() < 0.85,
        net_laden=rng.random() < 0.3,
        maandtarget_kw=rng.choice([2.5, 5.929]),
        maxphase_a=rng.choice([5, 18, 25, 27, 31]),
    )


@pytest.mark.skipif(ENV is None, reason="HA env not configured")
@pytest.mark.parametrize("seed", range(60))
def test_jinja_matches_python_model(seed):
    rng = random.Random(seed)
    inp = _rand_inputs(rng)
    rendered = _render(ENV, scenario_template(inp))
    if rendered is None:
        pytest.skip("HA unreachable")
    assert int(rendered) == decide_setpoint(inp), f"divergentie bij {inp}"
