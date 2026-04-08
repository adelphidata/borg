# Google Chat Setup

## 1. Create a Google Chat App

Go to the [Google Cloud Console](https://console.cloud.google.com/):

1. Create a new project (or use an existing one)
2. Enable the **Google Chat API**
3. Under **Configuration**, set up the Chat app:
   - **App name** and **description**
   - **Connection settings**: select **HTTP endpoint URL**
   - Set the URL to your gateway's webhook endpoint (see step 3 below)
4. Copy the **Verification Token** from the configuration page

## 2. Install via Borg

Credentials are stored in your OS keychain (macOS Keychain / Linux `secret-tool`) and wired into the settings database automatically. No manual file editing required.

### TUI (recommended)

```sh
borg
```

Type `/plugins`, find **Google Chat**, press Space to select, Enter to install, and paste the verification token when prompted.

### CLI

```sh
borg add google-chat
```

You will be prompted for:

- **Verification Token** — from Google Cloud Console > Chat API configuration

## 3. Set the Webhook URL

Expose a public URL (e.g. via ngrok):

```sh
ngrok http 7842
```

In the Google Chat API configuration, set the HTTP endpoint URL to:

```
https://your-domain.ngrok-free.app/webhook/google-chat
```

The gateway also accepts `/webhook/google_chat` and `/webhook/googlechat` as aliases.

## 4. Verify

Add the bot to a Google Chat space or send it a direct message. You should get a response from your agent.

## Features

- Direct messages and space messages
- Token verification
- Bot-to-space messaging

## Additional configuration

### Access control

```toml
[gateway.channel_policies]
google-chat = "open"   # trust Google Workspace auth
```
