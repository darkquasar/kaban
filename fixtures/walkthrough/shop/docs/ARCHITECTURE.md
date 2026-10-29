# How checkout works

## Checkout

Checkout builds a `Cart`, prices it with `price_lines`, then `CartService::apply_discount` applies the promotion rules. Discounts are never trusted from the client (see ADR 0007).

`apply_discount` verifies totals on the server. The architecture is the identifier reference for checkout.
