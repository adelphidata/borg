# Microsoft Teams Setup

## 1. Register a Bot in Azure

Go to the [Azure Portal](https://portal.azure.com/) and create a new **Bot Channels Registration** (or **Azure Bot** resource):

1. Create a new Azure AD App Registration
2. Note the **Application (client) ID** and create a **Client Secret**
3. Under **Channels**, add the **Microsoft Teams** channel

## 2. Install via Borg

Credentials are stored in your OS keychain (macOS Keychain / Linux `secret-tool`) and wired into the settings database automatically. No manual file editing required.

### TUI (recommended)

```sh
borg
```

Type `/plugins`, find **Teams**, press Space to select, Enter to install, and paste each credential when prompted.

### CLI

```sh
borg add teams
```

You will be prompted for:

- **App ID** — from Azure Portal > Bot registration
- **App Secret** — from Azure Portal > Bot registration > Certificates & secrets

## 3. Set the Messaging Endpoint

Expose a public URL (e.g. via ngrok):

```sh
ngrok http 7842
```

In the Azure Bot Configuration, set the **Messaging endpoint** to:

```
https://your-domain.ngrok-free.app/webhook/teams
```

## 4. Verify

Message the bot in Microsoft Teams. You should get a response from your agent.

## Features

- Direct messages and channel messages
- Service principal authentication (OAuth2 token exchange)
- Adaptive card support
- Automatic message chunking

## Additional configuration

### Access control

```toml
[gateway.channel_policies]
teams = "open"   # trust Teams workspace auth
```
