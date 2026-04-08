# Signal Setup

Signal integration uses `signal-cli` as a daemon. Messages are received via Server-Sent Events (SSE), not webhooks.

## 1. Install signal-cli

Install [signal-cli](https://github.com/AsamK/signal-cli):

```sh
# macOS
brew install signal-cli

# Linux (download from releases)
# See https://github.com/AsamK/signal-cli/releases
```

## 2. Register a Phone Number

Register a phone number with Signal via signal-cli:

```sh
signal-cli -a +1234567890 register
signal-cli -a +1234567890 verify CODE
```

## 3. Start the signal-cli Daemon

Run signal-cli in daemon mode:

```sh
signal-cli -a +1234567890 daemon --http
```

By default, the daemon listens on `localhost:8080`.

## 4. Install via Borg

Signal is installed through the TUI plugin marketplace. Credentials are stored in your OS keychain (macOS Keychain / Linux `secret-tool`) and wired into the settings database automatically.

```sh
borg
```

Type `/plugins`, find **Signal**, press Space to select, Enter to install, and enter your registered **phone number** (e.g. `+1234567890`) when prompted.

## 5. Verify

Send a message to the registered phone number on Signal. You should get a response from your agent.

## Features

- Direct messages
- Server-Sent Events (SSE) for real-time message reception
- Message deduplication
- Automatic message chunking

## Additional configuration

### Access control

```toml
[gateway.channel_policies]
signal = "pairing"   # pairing (default) | open | disabled
```

### signal-cli connection

Override the signal-cli daemon location if it is not running on the default `localhost:8080`:

```sh
borg settings set gateway.signal_cli_host "localhost"
borg settings set gateway.signal_cli_port 8080
```
