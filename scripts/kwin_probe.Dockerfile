FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        dbus \
        dbus-x11 \
        kwin-wayland \
        libcap2-bin \
        procps \
        python3 \
        qdbus-qt5 \
        kde-spectacle \
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

COPY kwin_probe_guest.sh /opt/kwin-probe/kwin_probe_guest.sh
RUN chmod 0755 /opt/kwin-probe/kwin_probe_guest.sh

ENTRYPOINT ["/opt/kwin-probe/kwin_probe_guest.sh"]
