#!/bin/sh
# Print the passphrase for a debug borgd's keychain copy (BORG_DEV_KEK_CACHE),
# creating it on first use. A debug build is ad-hoc signed, so the keychain
# treats every rebuild as a new app and asks for the login password again; the
# copy is what lets a rebuilt daemon boot without asking. The passphrase lives
# outside ~/.borg so a copy of that directory does not carry it.
set -eu
pass_file="$HOME/.config/borg-dev/kek-cache.pass"
if [ ! -s "$pass_file" ]; then
  mkdir -p "$(dirname "$pass_file")"
  chmod 700 "$(dirname "$pass_file")"
  (umask 077 && openssl rand -base64 32 > "$pass_file")
fi
cat "$pass_file"
