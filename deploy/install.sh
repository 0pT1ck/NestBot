#!/bin/sh
set -eu
# Run as root from the release directory containing nestbot and deploy/.
test "$(id -u)" -eq 0 || { echo 'Run as root.' >&2; exit 1; }
test -f ./nestbot || { echo 'Place the Linux binary beside deploy/.' >&2; exit 1; }
id nestbot >/dev/null 2>&1 || useradd --system --home-dir /var/lib/nestbot --shell /usr/sbin/nologin nestbot
install -m 0755 ./nestbot /usr/local/bin/nestbot
install -d -m 0750 -o root -g nestbot /etc/nestbot
install -d -m 0700 -o nestbot -g nestbot /var/lib/nestbot /var/cache/nestbot /run/nestbot
if [ ! -f /etc/nestbot/nestbot.toml ]; then install -m 0640 -o root -g nestbot deploy/nestbot.linux.toml /etc/nestbot/nestbot.toml; fi
if [ ! -f /etc/nestbot/secrets.env ]; then install -m 0640 -o root -g nestbot config/secrets.example.env /etc/nestbot/secrets.env; fi
install -m 0644 deploy/nestbot.service /etc/systemd/system/nestbot.service
systemctl daemon-reload
echo 'Installed. Edit /etc/nestbot/nestbot.toml and secrets.env, then initialize/login as nestbot and start the service.'
