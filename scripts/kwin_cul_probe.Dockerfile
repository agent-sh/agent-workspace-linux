FROM agent-workspace-linux-kwin-probe:ubuntu-26.04
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        python3-pyqt6 pipewire wireplumber \
        xdg-desktop-portal xdg-desktop-portal-kde \
    && rm -rf /var/lib/apt/lists/*
