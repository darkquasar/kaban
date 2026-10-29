# ADR 0007

## Decision

Discounts are computed server-side, never in the client. Client totals are never trusted from the client.

## Rationale

Tampered client totals must not bypass the promotion rules.
