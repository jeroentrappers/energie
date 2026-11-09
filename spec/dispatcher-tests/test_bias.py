"""Tests for the integral bias compensation (decide.next_bias) + its Jinja parity."""

from __future__ import annotations

import json
import os
import random
import urllib.request

import pytest

from dispatcher.decide import BIAS_LIMIT_W, Inputs, decide_setpoint, next_bias

from dispatcher.live_template import bias_scenario_template

MODES = ["Uit", "Zon only", "6A — van net", "6A — van batterij", "Max onder piek"]


def make(**kw) -> Inputs:
    """A neutral midday idle state that IS in the base-tracking regime."""
    base = dict(
        battery_power_net=0.0, p1=0.0, soc=60.0, is_ochtend=False,
        ev_laadmodus="Uit", ev_higher_hungry=False,
        inputs_valid=True, bias=0.0, net_laden=False, maandtarget_kw=5.929,
    )
    base.update(kw)
    return Inputs(**base)


# ---- behaviour (KI=0.1 slow integral, 20 W deadband) ------------------------
def test_accumulates_slowly_toward_killing_import():
    assert next_bias(make(p1=100, bias=0)) == 10.0
    assert next_bias(make(p1=100, bias=10)) == 20.0


def test_sign_export_lowers_bias():
    assert next_bias(make(p1=-200, bias=0)) == -20.0


def test_deadband_freezes_small_residual():
    # |p1| < 20 W → treated as noise, bias held
    assert next_bias(make(p1=15, bias=25)) == 25.0
    assert next_bias(make(p1=0, bias=37.5)) == 37.5


def test_corrects_residual_above_deadband():
    # a steady 30 W import (was inside the old 50 W dead zone) now gets nudged out
    assert next_bias(make(p1=30, bias=25)) == 28.0


def test_clamped_to_limit():
    assert next_bias(make(p1=5000, bias=190)) == BIAS_LIMIT_W
    assert next_bias(make(p1=-5000, bias=-190)) == -BIAS_LIMIT_W


@pytest.mark.parametrize("kw", [
    dict(inputs_valid=False),                                # fail-safe hold
    dict(soc=40),                                            # at discharge floor
    dict(soc=100),                                           # at charge ceiling
    dict(ev_higher_hungry=True, ev_laadmodus="6A — van net"),       # yielding to EV
    dict(ev_higher_hungry=True, ev_laadmodus="6A — van batterij"),  # yielding to EV
    dict(net_laden=True, soc=50),                            # grid-charge override
])
def test_frozen_outside_base_tracking(kw):
    assert next_bias(make(p1=900, bias=25, **kw)) == 25.0


def test_tracks_when_no_higher_ev_hungry():
    # a 6A mode but no higher EV hungry → plain self-consumption → bias tracks
    assert next_bias(make(p1=100, bias=0, ev_laadmodus="6A — van batterij",
                          ev_higher_hungry=False)) == 10.0


def test_bias_added_to_setpoint():
    assert decide_setpoint(make(p1=0, battery_power_net=0, bias=50)) == 50
    assert decide_setpoint(make(p1=0, battery_power_net=0, bias=0)) == 0


# ---- HA parity (skips when HA unreachable) ----------------------------------
def _load_env():
    path = os.path.expanduser("~/.config/ha/env")
    if not os.path.exists(path):
        return None
    env = {}
    for line in open(path):
        if "=" in line:
            k, v = line.strip().split("=", 1)
            env[k] = v
    return env if "HA_URL" in env and "HA_TOKEN" in env else None


def _render(env, template):
    req = urllib.request.Request(
        env["HA_URL"].rstrip("/") + "/api/template",
        data=json.dumps({"template": template}).encode(),
        headers={"Authorization": "Bearer " + env["HA_TOKEN"], "Content-Type": "application/json"},
        method="POST")
    try:
        return urllib.request.urlopen(req, timeout=10).read().decode()
    except Exception:
        return None


ENV = _load_env()


def _rand(rng):
    return Inputs(
        battery_power_net=rng.choice([-3000, -1000, 0, 1000, 3000]),
        p1=rng.randint(-5000, 5000),
        soc=rng.choice([10, 15, 30, 40, 41, 55, 90, 96, 99, 100]),
        is_ochtend=rng.random() < 0.3,
        ev_laadmodus=rng.choice(MODES),
        ev_higher_hungry=rng.random() < 0.5,
        inputs_valid=rng.random() < 0.85, bias=rng.choice([-200, -55.5, 0, 12.5, 200]),
        net_laden=rng.random() < 0.3, maandtarget_kw=rng.choice([2.5, 5.929]),
        maxphase_a=rng.choice([5, 18, 27, 31]),
    )


@pytest.mark.skipif(ENV is None, reason="HA env not configured")
@pytest.mark.parametrize("seed", range(60))
def test_bias_jinja_matches_python(seed):
    inp = _rand(random.Random(seed))
    rendered = _render(ENV, bias_scenario_template(inp))
    if rendered is None:
        pytest.skip("HA unreachable")
    assert float(rendered) == next_bias(inp), f"divergentie bij {inp}"
