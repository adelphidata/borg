---
name: healthcheck
description: "Audit host security posture — SSH config, firewall, listening ports, package updates, cron, and risky permissions"
triggers:
  - "audit this host"
  - "check ssh hardening"
  - "what ports are exposed"
  - "is this server secure"
---

# Host Healthcheck

Read-only audit. Surface findings; propose fixes for user to confirm before
running anything state-changing — extra care with SSH/firewall/sudoers
(lockout risk).

Detect platform:

```bash
uname -s
[ -r /etc/os-release ] && . /etc/os-release && echo "$ID $VERSION_ID"
sw_vers 2>/dev/null
```

## SSH

`sshd -T` shows the effective config (post-Match):

```bash
sudo sshd -T 2>/dev/null | grep -iE '^(permitrootlogin|passwordauthentication|pubkeyauthentication|permitemptypasswords|x11forwarding|allowusers|allowgroups|port)'
```

Critical: `permitrootlogin yes`, `permitemptypasswords yes`, password auth on
a public host.

Recent auth:

```bash
sudo last -F | head -20
sudo journalctl -u ssh -u sshd --since "24 hours ago" 2>/dev/null | grep -iE 'failed|invalid' | tail -20
log show --predicate 'process == "sshd"' --last 1d 2>/dev/null | grep -iE 'failed|invalid' | tail -20  # macOS
```

## Listening ports

```bash
ss -ltnup 2>/dev/null || netstat -tlnp        # Linux
lsof -nP -iTCP -sTCP:LISTEN                   # macOS
```

Anything bound `0.0.0.0`/`::` the user didn't ask for is a finding.

## Firewall

```bash
sudo ufw status verbose 2>/dev/null
sudo firewall-cmd --state 2>/dev/null && sudo firewall-cmd --list-all
sudo nft list ruleset 2>/dev/null
sudo iptables -L -n -v 2>/dev/null
/usr/libexec/ApplicationFirewall/socketfilterfw --getglobalstate   # macOS
sudo pfctl -s info 2>/dev/null                                     # macOS
```

VPS target: default-deny inbound, explicit allow for SSH + intended services.

## Updates

```bash
apt list --upgradable 2>/dev/null | grep -i security
dnf check-update --security 2>/dev/null
softwareupdate -l 2>/dev/null
systemctl is-enabled unattended-upgrades 2>/dev/null
```

## Disk encryption

```bash
fdesetup status                                       # macOS
lsblk -o NAME,TYPE,FSTYPE,MOUNTPOINT | grep -i crypt  # Linux LUKS
```

## Scheduled jobs

```bash
crontab -l 2>/dev/null
sudo ls -la /etc/cron.{d,daily,hourly,weekly,monthly}/ 2>/dev/null
systemctl list-timers --all 2>/dev/null
launchctl list | grep -v com.apple   # macOS
```

Watch for unfamiliar entries, root jobs fetching from network, `curl | sh`.

## Risky permissions

```bash
sudo find /etc /usr/local /opt -xdev -type f -perm -o+w 2>/dev/null | head -50
sudo find /usr /bin /sbin -xdev \( -perm -4000 -o -perm -2000 \) -type f 2>/dev/null
```

## Borg-specific

```bash
ls -ld ~/.borg ~/.borg/secrets 2>/dev/null   # 0700
ls -l ~/.borg/secrets/ 2>/dev/null            # 0600
borg status
```

If `kek_backend=file`, confirm `BORG_KEK_PASSPHRASE` comes from a 0600
systemd `EnvironmentFile=`, not a shell rc.

## Severity

- **Critical** — public root SSH, public password auth, world-writable system
  binary, no firewall + open admin services.
- **Important** — pending security updates, unattended-upgrades off on a
  server, FileVault/LUKS off on a portable device.
- **Info** — non-default SSH port, services bound to LAN only.

For each finding, propose the exact remediation and call out lockout risk.
Never auto-apply.
