# workdir browser curated image (spec §12): Chromium + Playwright + VNC/noVNC.
# Min shape 2 vCPU / 4 GB / 16 GB. Built into ext4 by deploy/build-image.sh.
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
# Playwright (npm) drives the system Google Chrome via channel:'chrome' / CDP;
# don't download Playwright's own browser bundle (keeps the image lean).
ENV PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1
RUN apt-get update && apt-get install -y --no-install-recommends \
      bash coreutils curl wget ca-certificates git socat iproute2 iputils-ping \
      python3 python3-pip python3-venv nodejs npm \
      xvfb fluxbox x11vnc novnc websockify x11-utils imagemagick \
      fonts-liberation \
    && wget -q https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb -O /tmp/chrome.deb \
    && apt-get install -y --no-install-recommends /tmp/chrome.deb && rm /tmp/chrome.deb \
    && npm i -g playwright \
    && rm -rf /var/lib/apt/lists/*
COPY sandbox-guest-agent /usr/local/bin/sandbox-guest-agent
COPY sandbox-init /sbin/sandbox-init
RUN chmod +x /usr/local/bin/sandbox-guest-agent /sbin/sandbox-init && mkdir -p /workspace
