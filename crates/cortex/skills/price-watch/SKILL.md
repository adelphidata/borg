---
name: price-watch
description: "Watch a product, flight, hotel, or ticket for a target price or for stock, and alert only when the condition is met. Pins the exact item, takes a live baseline, then checks on a schedule without duplicate alerts. For a one-off price check, use web_fetch instead."
category: productivity
triggers:
  - "alert me when the price drops"
  - "watch this price"
  - "tell me when it's back in stock"
  - "track this flight fare"
  - "set a price alert"
---

# Price Watch

Setup runs once with the user; each later check runs as a `schedule` job. Each
watch keeps one state file: `~/.borg/price-watch/<slug>.json`.

## Setup (once, with the user)

### 1. Pin the item

Source URL, product or listing ID, variant (size, colour, storage), quantity,
seller, condition, location, dates, travellers or guests, cabin or room class,
and acceptable substitutes. Two variants must not be confusable.

### 2. Pin the condition

Currency; all-in (tax, fees, shipping) or base price; maximum price; stock
rule; refundability; cooldown between alerts (default 24h); where to alert.
Try it on two made-up prices: the yes or no must be obvious.

### 3. Take a live baseline

Fetch the page now with `web_fetch`. If the price is drawn by script or the
site blocks plain fetches, use the `browser` tool. Record the time, base
price, fees, total, stock, and terms. **Do not schedule until this works.**

```bash
mkdir -p ~/.borg/price-watch
cat > ~/.borg/price-watch/sony-xm6.json <<'EOF'
{
  "item": {"url": "https://example.com/p/123", "id": "123", "variant": "black", "seller": "Example"},
  "condition": {"currency": "USD", "all_in": true, "max": 300, "in_stock": true, "cooldown_h": 24},
  "job_id": null,
  "last_good": {"at": "2026-10-23T14:05:00Z", "base": 348.0, "fees": 0, "total": 348.0, "in_stock": true},
  "last_alert": null
}
EOF
```

### 4. Schedule the check

Pick a polite cadence: 6 to 12 hours for shops, 2 to 4 hours close to a fare
deadline. Respect the site's terms.

```json
{"action": "create", "type": "prompt", "name": "Price watch: Sony XM6",
 "prompt": "Use the price-watch skill to run one check for ~/.borg/price-watch/sony-xm6.json.",
 "schedule_type": "interval", "schedule_expr": "6h", "delivery_channel": "origin"}
```

Save the returned job id in `job_id`. The skill must stay enabled for the job
to find these steps.

## Each check

1. **Fetch and normalise.** Use the same method as the baseline. Split base,
   fees, shipping and tax, total, and stock. Convert currency only with a
   dated rate, and keep the original. A failed or odd fetch (error page,
   captcha, no price) means *unknown*: never write it over `last_good`.
2. **Decide.** Alert when the condition newly holds, a clearly lower price
   appears, or stock returns, as the user asked. Make a fingerprint from total,
   stock, and terms. If it equals `last_alert.fingerprint`, or the cooldown has
   not passed, stay silent.
3. **Alert or stay quiet.** An alert gives the exact item and variant, the
   all-in price in the source currency, stock and terms, the threshold, the
   check time, and the link. Never say stock is reserved. Send no "still
   watching" messages unless the user asked for them.
4. **Save state.** Update `last_good` after a good fetch, and `last_alert`
   (with its fingerprint) after an alert.

When the user buys, or the date passes, cancel the job (`schedule`,
`action: "cancel"`, with `job_id`) and delete the state file.

## Pitfalls

- Comparing a base fare with an all-in threshold.
- Alerting on the wrong size, seller, dates, or room terms.
- Checking so often that the site blocks you.
