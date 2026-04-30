---
name: stripe-link
description: "Fetch one-time payment credentials from a Stripe Link wallet to complete purchases on the user's behalf."
category: payments
triggers:
  - "buy this for me"
  - "make a purchase"
  - "pay with my Link wallet"
  - "checkout with Link"
requires:
  bins: ["link-cli"]
install:
  link-cli:
    npm: "@stripe/link-cli"
---

# Stripe Link CLI

Use Link CLI when the user asks Borg to make an online purchase. Link returns a single-use virtual card (or `shared_payment_token` for MPP merchants) tied to one approved spend — real card details are never exposed.

## Auth (one-time)

```bash
link-cli auth login --client-name "Borg" --format json
link-cli auth status --format json
```

Visit the printed URL, log in to Link, and enter the short phrase to approve.

## Purchase flow

1. **List payment methods.** Pick an `id` from the user's Link wallet.
   ```bash
   link-cli payment-methods list --format json
   ```
2. **Create a spend request.** `--context` MUST be ≥100 chars and explain who/what/why. `--amount` is in cents and ≤ 50000.
   ```bash
   link-cli spend-request create \
     --payment-method-id csmrpd_xxx \
     --merchant-name "Stripe Press" \
     --merchant-url "https://press.stripe.com" \
     --context "Purchasing 'Working in Public' from press.stripe.com on behalf of the user, who initiated this checkout through Borg." \
     --amount 3500 \
     --line-item "name:Working in Public,unit_amount:3500,quantity:1" \
     --total "type:total,display_text:Total,amount:3500" \
     --request-approval \
     --format json
   ```
3. **Poll until terminal.** User approves in the Link app; polling exits non-zero with `code: "POLLING_TIMEOUT"` if it stays pending.
   ```bash
   link-cli spend-request retrieve <id> --interval 2 --max-attempts 150 --include=card --format json
   ```
4. **Use the card.** Read `card.number`, `cvc`, `exp_month`, `exp_year`, `billing_address` from the approved response and submit the merchant's checkout form. For MPP merchants (HTTP 402 + Stripe payment method), use `--credential-type "shared_payment_token"` at create time and pay via:
   ```bash
   link-cli mpp pay https://merchant.example/api/buy \
     --spend-request-id <id> --method POST --data '{"amount":3500}' --format json
   ```

## Test mode

Append `--test` to `spend-request create` during development — provisions test card `4242424242424242` without spending real money.

## Notes

- Always confirm the merchant URL and total with the user before calling `create --request-approval`. Once approved, the credential is live.
- `auth status --format json` returns an `update` field when a newer `link-cli` is available; surface it to the user, don't auto-upgrade.
- Set `NO_UPDATE_NOTIFIER=1` in non-interactive contexts.
