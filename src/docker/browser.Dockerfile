# syntax=docker/dockerfile:1
# The server browser: Chrome with the browse extension and relay, on a virtual screen you can
# view (noVNC), so bots on a server use a real browser and you can take over for handoffs.
# Runs on the host's network; everything listens on 127.0.0.1 only. Its home (/data: Chrome's
# profile, browse's keys) is a folder on the server, so logins survive restarts and rebuilds.
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive HOME=/data
RUN apt-get update && apt-get install -y --no-install-recommends \
        bash ca-certificates curl gnupg jq procps tzdata xdg-utils \
        xvfb x11vnc novnc websockify fluxbox \
        fonts-liberation fonts-dejavu-core fonts-noto-color-emoji fonts-noto-cjk \
    && curl -fsSL https://dl.google.com/linux/linux_signing_key.pub \
        | gpg --dearmor -o /usr/share/keyrings/google-chrome.gpg \
    && echo "deb [arch=amd64 signed-by=/usr/share/keyrings/google-chrome.gpg] http://dl.google.com/linux/chrome/deb/ stable main" \
        > /etc/apt/sources.list.d/google-chrome.list \
    && apt-get update && apt-get install -y --no-install-recommends google-chrome-stable \
    && rm -rf /var/lib/apt/lists/* \
    && cat /proc/sys/kernel/random/uuid | tr -d - > /etc/machine-id

# browse (the relay; bots' images install the same version).
RUN curl -fsSL https://opencode.ai/browse/install \
        | HOME=/tmp/browse XDG_BIN_HOME=/usr/local/bin sh -s -- --no-setup \
    && rm -rf /tmp/browse && browse version

# Chrome installs the browse extension itself (and keeps it updated) by policy, and restores
# the last session on start, which keeps sites' session cookies (logins) across restarts.
RUN mkdir -p /etc/opt/chrome/policies/managed \
    && printf '%s\n' '{' \
        '  "ExtensionInstallForcelist": ["mfnicocicmmlkpjnaffgihfjhdgjkdjg;https://clients2.google.com/service/update2/crx"],' \
        '  "DefaultBrowserSettingEnabled": false,' \
        '  "RestoreOnStartup": 1,' \
        '  "MetricsReportingEnabled": false,' \
        '  "PromotionalTabsEnabled": false' \
        '}' > /etc/opt/chrome/policies/managed/dumbot.json

COPY --chmod=755 <<'EOF' /usr/local/bin/dumbot-browser
#!/bin/bash
# Starts the screen, the viewer, the relay and Chrome, keeps each running, and pairs the
# extension with the relay the first time.
set -u
export HOME=/data DISPLAY=:99 OPENCODE_BROWSER_AUTO_UPDATE=off
VIEW_PORT=${DUMBOT_VIEW_PORT:-7459}
VNC_PORT=${DUMBOT_VNC_PORT:-7460}
mkdir -p /data/chrome /data/logs
# Left behind by the last run, they'd stop the screen and Chrome from starting.
rm -f /tmp/.X99-lock /tmp/.X11-unix/X99 /data/chrome/SingletonLock /data/chrome/SingletonSocket /data/chrome/SingletonCookie

keep() { # name, command…: runs it again whenever it exits
    local name=$1; shift
    while true; do "$@" >>"/data/logs/$name.log" 2>&1; sleep 2; done
}

# Stopping (docker stop, a reboot): close Chrome properly, so it saves its cookies and session,
# before anything else goes.
stop() {
    trap - TERM INT
    kill $(jobs -p) 2>/dev/null
    pkill -TERM -o -x chrome 2>/dev/null
    for _ in $(seq 50); do pgrep -x chrome >/dev/null || break; sleep 0.5; done
    pkill -TERM -f 'browse serve' 2>/dev/null
    exit 0
}
trap stop TERM INT

keep screen Xvfb :99 -screen 0 "${DUMBOT_SCREEN:-1440x900x24}" -nolisten tcp &
for _ in $(seq 100); do [ -e /tmp/.X11-unix/X99 ] && break; sleep 0.1; done
keep wm fluxbox &
keep vnc x11vnc -display :99 -localhost -rfbport "$VNC_PORT" -forever -shared -nopw -quiet -noxdamage &
keep view websockify --web /usr/share/novnc "127.0.0.1:$VIEW_PORT" "127.0.0.1:$VNC_PORT" &
keep relay browse serve &
keep chrome google-chrome --user-data-dir=/data/chrome --no-first-run --no-default-browser-check \
    --password-store=basic --start-maximized --disable-dev-shm-usage &

# First run: pair the extension with the relay (its setup page takes the code in its link).
(
    for _ in $(seq 120); do
        state=$(browse status --json 2>/dev/null | jq -r '.pairingState // empty')
        [ -n "$state" ] && break; sleep 1
    done
    if [ "$state" = "not-seen" ] || [ "$state" = "unpaired" ]; then
        # The extension installs a few seconds after Chrome's first start.
        for _ in $(seq 60); do
            [ -d /data/chrome/Default/Extensions/mfnicocicmmlkpjnaffgihfjhdgjkdjg ] && break; sleep 1
        done
        sleep 3
        browse pair --wait-seconds 120 >/data/logs/pair.log 2>&1 &
        for _ in $(seq 30); do
            code=$(grep -o 'Pairing code: [A-Z0-9-]*' /data/logs/pair.log | cut -d' ' -f3)
            [ -n "$code" ] && break; sleep 1
        done
        google-chrome --user-data-dir=/data/chrome \
            "chrome-extension://mfnicocicmmlkpjnaffgihfjhdgjkdjg/welcome.html#code=$code" >/dev/null 2>&1
        wait
    fi
) &

wait
EOF

ENTRYPOINT ["/usr/local/bin/dumbot-browser"]
