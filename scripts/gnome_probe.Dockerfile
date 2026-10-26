FROM ubuntu:26.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    gnome-shell gnome-shell-common gsettings-desktop-schemas dbus dbus-x11 \
    xwayland xterm tmux x11-utils at-spi2-core python3 python3-gi gir1.2-gtk-4.0 bubblewrap procps \
    libasound2t64 libxkbcommon-x11-0 libvulkan1 libegl1 libfontconfig1 \
    libxcb-xfixes0 libxcb-randr0 libxcb-shape0 libxcb-xkb1 libxcb-render0 \
    libxcb-present0 libxcb-dri3-0 libx11-xcb1 wayland-utils \
    && rm -rf /var/lib/apt/lists/*
# ibus installs /etc/xdg/Xwayland-session.d/10-ibus-x11; the probe proves it never runs.
RUN apt-get update && apt-get install -y --no-install-recommends python3-pyqt6 python3-pil ibus && rm -rf /var/lib/apt/lists/*
COPY gnome_smoke.py gnome_fixture.py gnome_qt_fixture.py gnome_primary_reader.py kwin_mcp.py /opt/gnome-probe/
ENTRYPOINT ["python3","/opt/gnome-probe/gnome_smoke.py"]
