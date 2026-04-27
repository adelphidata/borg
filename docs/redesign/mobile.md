# Mobile App — v2

> **Status: deferred to v2.** A first-party native mobile app is not in v1 scope. Hobbyists reach Borg from a phone via the gateway — any OpenAI-compat client (ChatBox, Open WebUI, LobeChat), an MCP client, or a channel adapter (Telegram / Slack / Discord / Signal / iMessage / Teams / SMS) covers the chat surface in v1. Device-side capabilities (camera, mic, biometric, push) wait for the v2 native app.

This file is preserved as a v2 reference. The full prior design (PAKE pairing UX, capability runtime, offline queue, push payload model, multi-profile, biometric gating) lives in git history at `docs/redesign/mobile.md` prior to this rewrite — restore it when v2 starts and update against the v1-as-shipped reality.

## v2 scope (placeholder)

When v2 starts, this doc will define:

- iOS + Android native apps as gRPC-Web clients
- PAKE pairing carrying the v1 flow from `auth-and-onboarding.md`
- Capability runtime (camera, mic, location, biometric, clipboard) advertising into the v1 capability protocol
- Push notifications via APNs/FCM with flag-only payload (body fetched over mTLS on tap)
- Offline outbound queue
- Biometric gating for app open + sensitive actions
- Multi-daemon profile switching mirroring the TUI's named profiles

## v3 cross-cutting (when both ship)

The v3 Sentry-mode mobile additions (Derived PIV credential per NIST SP 800-157, GFE-only distribution via agency MDM, agency push relay alternative to APNs/FCM, classification-tagged capability results) compose with the v2 base. They do not require v2 to ship first; they require v3 to ship.

## References

- Companion: [`architecture-redesign.md`](architecture-redesign.md) — capability protocol, gRPC-Web, resumable event log
- Companion: [`auth-and-onboarding.md`](auth-and-onboarding.md) — PAKE pairing, multi-profile
- NIST SP 800-157 (Derived PIV Credentials) — for v3 Sentry mobile
