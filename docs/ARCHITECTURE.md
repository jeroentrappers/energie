# Architecture

```
                 ┌───────────────── crates/app (the energie binary) ─────────────────┐
 devices  ─read─▶│ drivers ─▶ World ─▶ energie-core ─▶ Commands ─▶ gate ─▶ drivers   │─write─▶ devices
 cloud APIs      │                       (pure logic)                                │
                 │ dashboard + API · MQTT bridge · history · shadow · persistence    │
                 └───────────────────────────────────────────────────────────────────┘
```

| Crate | Responsibility |
| --- | --- |
| `crates/core` | The control logic. Pure: World → derived sensors → automations → commands. No I/O, so it is fully testable on a virtual clock. |
| `crates/drivers` | Device protocols, the plug-in contract, and protocol-level simulators. |
| `crates/cloud` | Clients for cloud services. |
| `crates/app` | The binary: runtime loop, control gate, web server, MQTT bridge, history store, Home Assistant link, shadow comparison, replay and the simulated house. |

## One tick

Once a second the runtime:

1. Reads each device's latest cached section. A tick never waits on a device: every device has
   its own task that polls it and applies queued commands.
2. Builds the `World` and runs the engine, which returns commands and notifications.
3. Passes each command through the control gate: kill switch, rate limit, and "no reading yet"
   checks.
4. Records it in the audit log and verifies it against later readings.

## Design choices

- **Pure core.** Decision logic never touches a clock or socket, which is what makes the
  simulated-day tests and replays possible.
- **Read-only by default.** A driver only gets an actuator if the config sets
  `read_only = false` and gives it a `control` role, outside shadow mode.
- **Fail toward the device's own behaviour.** Forced battery modes always carry a hardware
  timeout.
- **Faithful to its origin.** The logic was ported from a Home Assistant configuration, which
  explains the Jinja-compatible rounding helpers, the Dutch identifiers and the `// LIVE:`
  comments on inherited quirks. [`PORTING.md`](PORTING.md) has the rules and
  [`FINDINGS.md`](FINDINGS.md) the quirks.

## Where to read more

| Topic | Document |
| --- | --- |
| The control logic and its reasoning | [`spec/LOGIC.md`](../spec/LOGIC.md) |
| Device registers, safety mechanisms | [`CONTROL.md`](CONTROL.md) |
| Runtime internals, HTTP API, history format | [`crates/app/README.md`](../crates/app/README.md) |
| Driver protocols | [`crates/drivers/README.md`](../crates/drivers/README.md) |
| Cloud clients | [`crates/cloud/README.md`](../crates/cloud/README.md) |
| How the tests were reviewed | [`TEST-REVIEW.md`](TEST-REVIEW.md), [`MUTANTS.md`](MUTANTS.md) |
