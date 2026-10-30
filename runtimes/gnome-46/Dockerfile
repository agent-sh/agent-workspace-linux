FROM ubuntu@sha256:534baea6a22c03a63003dbc8dbe78fe34bc0d7e595d9a9dc9834884ff530eb55
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends gnome-shell gnome-shell-common gsettings-desktop-schemas dbus dbus-x11 xwayland xterm tmux x11-utils xdotool xauth xclip openbox xvfb at-spi2-core python3 python3-gi python3-pil gir1.2-gtk-4.0 bubblewrap procps libasound2t64 libxkbcommon-x11-0 libvulkan1 libegl1 libfontconfig1 libxcb-xfixes0 libxcb-randr0 libxcb-shape0 libxcb-xkb1 libxcb-render0 libxcb-present0 libxcb-dri3-0 libx11-xcb1 wayland-utils xdg-desktop-portal-gnome pipewire wireplumber ca-certificates curl patchelf
RUN gnome-shell --version
