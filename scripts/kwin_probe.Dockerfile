FROM ubuntu:26.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        dbus \
        dbus-x11 \
        kwin-wayland \
        kwin-common \
        kwin-style-breeze \
        breeze-cursor-theme \
        dbus-user-session \
        at-spi2-core \
        libcap2-bin \
        procps \
        python3 \
        qdbus-qt6 \
        kde-spectacle \
        qt6-wayland \
        wl-clipboard \
        kwrite \
        libasound2t64 \
        libxkbcommon-x11-0 \
        libvulkan1 \
        libegl1 \
        libfontconfig1 \
        libxcb-xfixes0 \
        libxcb-randr0 \
        libxcb-shape0 \
        libxcb-xkb1 \
        libxcb-render0 \
        libxcb-present0 \
        libxcb-dri3-0 \
        libx11-xcb1 \
        wayland-utils \
        weston \
        wtype \
        x11-apps \
        x11-utils \
        xdotool \
        xvfb \
        xwayland \
    && setcap -r /usr/bin/kwin_wayland \
    && rm -rf /var/lib/apt/lists/*

COPY kwin_probe_guest.sh kwin_smoke.py /opt/kwin-probe/
RUN chmod 0755 /opt/kwin-probe/kwin_probe_guest.sh

ENTRYPOINT ["/opt/kwin-probe/kwin_probe_guest.sh"]
