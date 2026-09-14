# Source and release scope

This fork retains the existing execution engine and adds optional, general copying policies. The new settings and their limits are described in [Configuration](CONFIGURATION.md).

## Changes in the proportional-copy branch

- Optional share-based percentage sizing, including cumulative partial fills and recovery in the same units.
- Optional opposite-outcome copying while retaining identity, duplicate-fill, persistence and spending checks.
- Durable source-fill identities for share mode, including restart and write-ahead-log recovery tests.
- Existing decoder, feed-liveness and cash-reservation fixes carried into the development base. Decoder tests use synthetic order payloads.
- Compatibility defaults for old configurations; examples stay disabled and dry.

These are runtime changes and require regression checks. Earlier source-export equivalence checks described the original packaging baseline, not this branch. No branch build is automatically deployed, armed, funded or published.

## Preserved components

Order signing, network transports, execution, accounting, redemption and the dashboard remain part of one engine. Existing price limits, exposure limits and optional settlement gates are not automatically widened by the new policy. Dependencies remain pinned by the existing lockfile.

## Excluded material

The public source must not include populated credentials or wallet configuration, selected trader identities, private research, recorded chain payloads, trading tapes, portfolio snapshots, PnL reports, operator logs or deployment-specific details. Only synthetic, self-contained regression fixtures belong in the distributed test suite. Historical private recordings can be used outside the source tree for local verification.

Run the release privacy check and an independent credential scanner before publishing. Operator state, research and runtime data must stay outside versioned source. Passing those checks is not evidence of trading profitability or execution quality.
