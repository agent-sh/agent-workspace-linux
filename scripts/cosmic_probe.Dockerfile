FROM ubuntu:26.04
ENV DEBIAN_FRONTEND=noninteractive
ADD --checksum=sha256:a3bfce18d53aa9d635d899f1f555e84bbbf2c859d80cef3b4eb1c36b2c772026 https://archive.ubuntu.com/ubuntu/pool/universe/libd/libdisplay-info/libdisplay-info1_0.1.1-2build1_amd64.deb /opt/libdisplay-info1.deb
# Official Pop packages, pinned to the audited compositor and package digest.
ADD --checksum=sha256:f493771502e56245158906e92b4da0b5a7f0bc2fb1926f3feb81d6294541b427 https://apt.pop-os.org/release/pool/noble/cosmic-comp/8cb1802caada438bb5cc7670dfabdaca5fad0502/cosmic-comp_0.1~1792508925~24.04~8cb1802_amd64.deb /opt/cosmic-comp.deb
RUN apt-get update && apt-get install -y --no-install-recommends \
    /opt/cosmic-comp.deb /opt/libdisplay-info1.deb xwayland \
    weston dbus dbus-x11 at-spi2-core bubblewrap xauth x11-xserver-utils \
    x11-utils xterm tmux wl-clipboard python3 python3-pyqt6 qt6-wayland \
    libasound2t64 libxkbcommon-x11-0 libvulkan1 libegl1 libfontconfig1 \
    libxcb-xfixes0 libxcb-randr0 libxcb-shape0 libxcb-xkb1 libxcb-render0 \
    libxcb-present0 libxcb-dri3-0 libx11-xcb1 procps \
    && rm -rf /var/lib/apt/lists/*
COPY cosmic_smoke.py cosmic_fixture.py kwin_mcp.py runtime_isolation_smoke.py /opt/cosmic-probe/
ENTRYPOINT ["python3", "/opt/cosmic-probe/cosmic_smoke.py"]
