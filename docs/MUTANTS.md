# Mutation testing: `energie-core`

`cargo mutants -p energie-core -j 4` (cargo-mutants 27), 2026-11-07.

| Caught | Missed | Unviable | Timeout | Total |
| ---: | ---: | ---: | ---: | ---: |
| 1923 | 24 | 69 | 6 | 2022 |

The 6 timeouts (`meters.rs` `next_reset_after`, `reset_due`, `Integrator::update`;
`time.rs` `quarter_start`) are mutants that make a reset or integration loop run forever:
the tests hang instead of passing, so they count as detected.

Every one of the 12 missed mutants is listed below as an equivalent: it cannot change anything HA or the port
can observe. All other survivors of the first run were killed by tests that pin the live
behaviour at the boundary (the operator as written in `the earlier configurationautomations.yaml`
or `templates.txt`), plus astral reference values for `sun.rs`.

## Port bug found

`jinja::round_floor` (`x | round(n, 'floor')`) snapped products within 1e-9 of an integer up
before flooring. HA's `forgiving_round` is plain `math.floor(x * 10**n) / 10**n`, so
representation error is kept: `2.3 | round(2, 'floor')` is 2.29 in HA. The modulators hit
this: `avail = ((besch + batt) / 230 + act) * k | round(1, 'floor')` with besch 2300 W,
act 4 A, k 1.4 is 19.599999999999998 → HA 19.5 A, the port said 19.6 A. Fixed to match HA
(`round_floor_floors_the_product_like_ha`).

## Equivalent mutants

| Mutant | Why it cannot be observed |
| --- | --- |
| `battery.rs` `bias_tracking`: `!(net_laden && soc < 100)` `<` → `<=` | Differs only at `soc == 100`, where the next conjunct `soc < 100` is false anyway. |
| `jinja.rs` `round`: `n < 0` → `<=` | At `n = 0` the negative-precision path computes `round_ties_even(x / 1) · 1`, which is exactly Python's `round(x)` too. |
| `derived.rs` `Kwartier toegestaan vermogen`: `credit > 0` → `>=` | At `credit == 0` both branches add 0 (`0 · 0.65` vs `0`). |
| `jinja.rs` `py_float`: `x > 0.0` → `>=` | Only reached for ±infinity, which is never 0. |
| `jinja.rs` `py_float`: `exp < 0` → `<=` | Scientific notation is only used for \|x\| < 1e-4 or ≥ 1e16, whose decimal exponent is never 0. |
| `ev_hoger_laadstil.rs`: `sinds > 0` → `>=` | `sinds == 0` is taken by the preceding `else if sinds == 0.0` branch. |
| `ev_laden_buzz.rs` `actions`: `override.unwrap_or(-1.0)` → `unwrap_or(1.0)` | Only reached with `target >= 6`, so \|−1 − target\| and \|1 − target\| are both ≥ 5 ≥ 0.05: an unavailable override is always re-sent. |
| `ev_laden_cable.rs` `actions`: `set.unwrap_or(-1.0)` → `unwrap_or(1.0)` | The compared amps are ≥ 6, equal to neither −1 nor 1. |
| `piekhistoriek.rs`: `from.unwrap_or(-1.0) > 0` → `>= 0` | `from == 0` would need `to < 0`, which `to >= 0` excludes. |
| `piekhistoriek.rs`: `from.unwrap_or(-1.0)` → `unwrap_or(1.0)` | With `from` unavailable the last conjunct reads `to < 0` (`from|float(0)`), which `to >= 0` excludes. |
| `piekhistoriek.rs`: `to < from` → `<=` | Inside `if from != to`; with both available they differ, and with either unavailable an earlier conjunct is already false. |
| `sun.rs` `elevation`: `true_solar_min / 4 - 180` → `+ 180` | The hour angle only enters through `cos`, and the two angles are 360° apart. |
| `sun.rs` `refraction`: `elevation > 5.0` / `> -0.575` → `>=` | The branch boundaries: an elevation exactly 5.0° or −0.575° bit for bit never occurs at a tick. |
| `sun.rs` `crossing`: `< target`, `> target`, `> 1 ms` → `<=`/`>=` | The bisection's tie-breaking and stop rule: they move the found instant by at most 1 ms. |
| `sun.rs` `sampled_above`: `round(e, 2) > −0.833` → `>=` | A value rounded to 0.01° can never equal −0.833. |
| `sun.rs` `above_horizon`: `now < dawn`, `now < sunrise`, `now < sunset` → `<=` / `==`; `\|\|` → `&&` | The sampled form is total: before dawn or after dusk `sampled_above` reads samples that are deep below the horizon, and between sunrise and sunset it reads samples that are above it, so routing those instants through the other branch gives the same state. |

The `::id` mutants of automations are killed by the registry test that pins the 29 enabled live ids.
