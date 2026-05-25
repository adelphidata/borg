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

Returns a single-use virtual card (or `shared_payment_token` for MPP
merchants) tied to one approved spend. Confirm merchant URL + total with
the user before `--request-approval` — once approved the credential is live.
Set `NO_UPDATE_NOTIFIER=1` for non-interactive runs.

## Auth (one-time)

```bash
link-cli auth login --client-name "Borg" --format json
link-cli auth status --format json    # exposes `update` field; surface, don't auto-upgrade
```

## Purchase flow

```bash
# 1. List wallet
link-cli payment-methods list --format json

# 2. Create spend request
#    --context MUST be ≥100 chars; --amount in cents (≤ 50000)
#    Append --test for test card 4242424242424242
link-cli spend-request create \
  --payment-method-id csmrpd_xxx \
  --merchant-name "Stripe Press" \
  --merchant-url "https://press.stripe.com" \
  --context "Purchasing 'Working in Public' from press.stripe.com on behalf of the user, who initiated this checkout through Borg." \
  --amount 3500 \
  --line-item "name:Working in Public,unit_amount:3500,quantity:1" \
  --total "type:total,display_text:Total,amount:3500" \
  --request-approval --format json

# 3. Poll (user approves in Link app); exits POLLING_TIMEOUT if stuck pending
link-cli spend-request retrieve <id> --interval 2 --max-attempts 150 --include=card --format json

# 4a. Standard checkout: read card.number/cvc/exp_*/billing_address from response,
#     submit the merchant's form.

# 4b. MPP merchants (HTTP 402 + Stripe payment method):
#     use --credential-type "shared_payment_token" at create, then:
link-cli mpp pay https://merchant.example/api/buy \
  --spend-request-id <id> --method POST --data '{"amount":3500}' --format json
```
