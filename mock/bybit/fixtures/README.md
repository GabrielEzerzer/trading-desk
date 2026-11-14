# Venue response fixtures

The shapes the mock venue answers with, from the venue's v5 documentation, read on 2026-11-14.

- `order-create.json`, `order-amend.json`, `order-cancel.json`, `market-time.json` and
  `instruments-info.json` are the response examples on
  <https://bybit-exchange.github.io/docs/v5/order/create-order>, `.../order/amend-order`,
  `.../order/cancel-order`, `.../market/time` and `.../market/instrument` (the linear example),
  unchanged.
- `order-realtime.json`, `order-history.json`, `execution-list.json`, `position-list.json`,
  `wallet-balance.json` and `market-tickers.json` carry the response fields listed on
  `.../order/open-order`, `.../order/order-list`, `.../order/execution`, `.../position`,
  `.../account/wallet-balance` and `.../market/tickers`, with made-up values for a linear USDT
  perpetual (the pages' own examples use other categories).
- `error-rate-limit.json` is the `retCode` 10006 answer quoted on `.../rate-limit`.

`td-orders` parses each of them in its tests, and the mock's tests check that every field it sends
appears in the matching fixture.
