# Porting rules

The Rust code reproduces what the live Home Assistant install does, not what the older docs or
the Python mirror say it should do.

1. **The original Home Assistant configuration was the spec.** That configuration is private to
   the house it ran in and isn't part of this repository. `spec/LOGIC.md` explains *why*; `spec/decide.py` and `spec/dispatcher-tests/` are an older
   Python mirror of the battery brain (it predates the reservation and trickle rules). Where
   they disagree with the live Jinja, the live Jinja wins.
2. **Port faithfully, flag what looks wrong.** If live behaviour looks like a bug (a unit
   mix-up, a dead branch, a threshold that can never fire), reproduce it, add a `// LIVE:`
   comment that says what looks wrong and why, and list it in `docs/FINDINGS.md`. Fixing it is
   a separate, explicit decision.
3. **HA semantics to keep:**
   - `states(x)|float(d)` is `x.unwrap_or(d)`; `has_value(x)` is `x.is_some()`.
   - Jinja `round` ties to even (`jinja::round`), `round(n, 'floor')` is `jinja::round_floor`,
     `|int` truncates.
   - `numeric_state` / `template` / `state to:` triggers fire on a *transition*, after `for:`;
     a condition already true at start does not fire (`trigger::Held`).
   - `time_pattern` and `time` triggers: `Cx::every` / `Cx::daily` (never on the start tick).
   - `homeassistant: start`: `Cx::started`.
   - `mode: single` + `max_exceeded: silent`: a trigger while the actions still run is
     dropped. Actions with `delay:` become a small state machine in the automation struct.
   - `last_triggered` is set when the actions run (`Cx::mark`), and only then.
   - `this.attributes.last_triggered` debounces read `Cx::since_triggered(self.id())`.
   - A template that reads `now()` keeps its last render until an entity it read changes or the
     minute turns (`core/src/render.rs`); automations at second 0 still see the old render.
   - A template sensor is only written when its value changes, so an `integration` over it sees
     an event per change (`Integrator::tick`), not per engine tick. Device-fed helpers (P1
     meters, the P1 median) update before the templates that read them.
4. **Shared predicates** ("is the Buzz plugged", "is power cheap", the phase views) live in
   `core/src/cascade.rs`. Use them instead of re-deriving.
5. **Tests prove behaviour, not lines.** Name each test for the rule it pins down and put the
   rule's source in a doc comment (automation id, template name, or a LOGIC.md section). Use
   numbers worked out by hand from the Jinja so a reviewer can check them. Coverage is a floor:
   100 % of lines of the files you own (`cargo llvm-cov -p <crate> --text --show-missing-lines`),
   with no coverage-exclusion attributes.
6. **Shared types.** Add a field to `World`, `Memory` or `Derived` only when you need it, append
   it at the end of the struct, and document the HA entity it stands for.
7. Clippy clean, `cargo fmt`.
