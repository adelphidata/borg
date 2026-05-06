---
name: healthcheck
description: "Audit host security posture — SSH config, firewall, listening ports, package updates, cron, and risky permissions"
triggers:
  - "audit this host"
  - "check ssh hardening"
  - "what ports are exposed"
  - "is this server secure"
---

# Host Healthcheck Skill

Read-only audit of the host running Borg. Surfaces findings; **does not** apply fixes — propose changes to the user, then make them yourself with separate `run_shell` calls only after they confirm. Especially careful with anything that could lock the user out (SSH, firewall, sudoers).

Detect the platform first:

```bash
uname -s              # Darwin | Linux
[ -r /etc/os-release ] && . /etc/os-release && echo "$ID $VERSION_ID"
sw_vers 2>/dev/null   # macOS only
```

## SSH Configuration

`sshd -T` prints the *effective* config (after Match blocks). Inspect the high-risk knobs:

```bash
sudo sshd -T 2>/dev/null | grep -iE '^(permitrootlogin|passwordauthentication|pubkeyauthentication|permitemptypasswords|x11forwarding|allowusers|allowgroups|port|protocol)'
```

Flag findings:
- `permitrootlogin yes` — should be `no` or `prohibit-password`.
- `passwordauthentication yes` on a public host — prefer key-only.
- `permitemptypasswords yes` — always wrong.
- Default port 22 with public exposure — note (not a bug, just signal).

Recent auth activity:

```bash
sudo last -F | head -20
sudo journalctl -u ssh -u sshd --since "24 hours ago" 2>/dev/null | grep -iE 'failed|invalid' | tail -20
# macOS:
log show --predicate 'process == "sshd"' --last 1d 2>/dev/null | grep -iE 'failed|invalid' | tail -20
```

## Listening Ports

```bash
# Linux
ss -ltnup 2>/dev/null || ss -ltnp || netstat -tlnp

# macOS
lsof -nP -iTCP -sTCP:LISTEN
```

For each non-loopback listener, decide: expected? Cross-check against known services (sshd, borgd, nginx, postgres). Anything bound to `0.0.0.0` / `::` that the user didn't ask for is a finding.

## Firewall

```bash
# Linux — try in order
sudo ufw status verbose 2>/dev/null
sudo firewall-cmd --state 2>/dev/null && sudo firewall-cmd --list-all
sudo nft list ruleset 2>/dev/null
sudo iptables -L -n -v 2>/dev/null

# macOS application firewall + pf
/usr/libexec/ApplicationFirewall/socketfilterfw --getglobalstate
sudo pfctl -s info 2>/dev/null
```

VPS posture target: default-deny inbound, explicit allow for SSH + intended services only.

## Package Updates

```bash
# Debian/Ubuntu
apt list --upgradable 2>/dev/null | tail -n +2 | wc -l
# Show security-flagged updates specifically
apt list --upgradable 2>/dev/null | grep -i security

# RHEL/Fedora
dnf check-update --security 2>/dev/null

# macOS
softwareupdate -l 2>/dev/null
```

Check unattended-upgrades / automatic updates are enabled on production hosts:

```bash
# Debian/Ubuntu
systemctl is-enabled unattended-upgrades 2>/dev/null
cat /etc/apt/apt.conf.d/20auto-upgrades 2>/dev/null
# macOS
defaults read /Library/Preferences/com.apple.SoftwareUpdate.plist 2>/dev/null
```

## Disk Encryption

```bash
# macOS
fdesetup status
# Linux (LUKS)
lsblk -o NAME,TYPE,FSTYPE,MOUNTPOINT | grep -i crypt
```

## Scheduled Jobs

```bash
crontab -l 2>/dev/null
sudo ls -la /etc/cron.{d,daily,hourly,weekly,monthly}/ 2>/dev/null
systemctl list-timers --all 2>/dev/null
# macOS
launchctl list | grep -v com.apple
```

Look for: unfamiliar entries, jobs running as root that fetch from the network, suspicious `curl | sh` patterns.

## Risky Permissions

```bash
# World-writable files outside /tmp and friends (slow on large disks — bound it)
sudo find /etc /usr/local /opt -xdev -type f -perm -o+w 2>/dev/null | head -50

# SUID/SGID survey on system paths
sudo find /usr /bin /sbin -xdev \( -perm -4000 -o -perm -2000 \) -type f 2>/dev/null
```

## Borg-Specific

```bash
ls -ld ~/.borg ~/.borg/secrets 2>/dev/null   # should be 0700
ls -l ~/.borg/secrets/ 2>/dev/null            # kek.enc, etc. should be 0600
borg status                                   # daemon health, posture, vitals
```

If `kek_backend=file`, confirm `BORG_KEK_PASSPHRASE` is sourced from a protected location (systemd `EnvironmentFile=` with `0600`, not a shell rc file).

## Reporting

Group findings by severity:

- **Critical** — root SSH login enabled with public exposure, password auth on a public host, world-writable system binary, no firewall + open admin services.
- **Important** — pending security updates, unattended-upgrades disabled on a server, FileVault/LUKS off on a portable device.
- **Informational** — non-default SSH port, services bound to LAN.

For each finding, propose the *exact* remediation command the user could run, and call out reversibility / lockout risk before suggesting it. Never run state-changing commands inside this skill — surface the diagnosis, let the agent loop decide whether to act after the user confirms.
