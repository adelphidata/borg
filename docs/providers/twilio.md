# Twilio Setup (SMS & WhatsApp)

Twilio integration supports both SMS and WhatsApp messaging.

## 1. Create a Twilio Account

Sign up at [twilio.com](https://www.twilio.com/) and note your **Account SID** and **Auth Token** from the dashboard.

## 2. Get a Phone Number

- **SMS**: Purchase a phone number in the Twilio console
- **WhatsApp**: Set up a WhatsApp sender in the Twilio console (or use the sandbox for testing)

## 3. Install via Borg

Credentials are stored in your OS keychain (macOS Keychain / Linux `secret-tool`) and wired into the settings database automatically. No manual file editing required.

### TUI (recommended)

```sh
borg
```

Type `/plugins`, find **SMS** and/or **WhatsApp**, press Space to select, Enter to install, and paste each credential when prompted. Both use the same Twilio credentials under the hood.

### CLI

```sh
borg add twilio
```

You will be prompted for:

- **Account SID** — from Twilio Console
- **Auth Token** — from Twilio Console
- **Phone Number** — your Twilio phone number (e.g. `+1234567890`)

## 4. Set the Webhook URL

Expose a public URL (e.g. via ngrok):

```sh
ngrok http 7842
```

In the Twilio console:

- **SMS**: Under your phone number's configuration, set the **Messaging webhook** to:
  ```
  https://your-domain.ngrok-free.app/webhook/sms
  ```
- **WhatsApp**: Under the WhatsApp sandbox or sender configuration, set the webhook to:
  ```
  https://your-domain.ngrok-free.app/webhook/whatsapp
  ```

The gateway also accepts `/webhook/twilio` as a generic endpoint for both.

## 5. Verify

Send an SMS to your Twilio number or a WhatsApp message to your WhatsApp-enabled number. You should get a response from your agent.

## Features

- SMS and WhatsApp messaging via Twilio API
- HMAC-SHA1 request signature verification
- Circuit breaker for reliability
- Automatic message chunking

## Additional configuration

### Access control

```toml
[gateway.channel_policies]
twilio = "pairing"   # pairing (default) | open | disabled
```
