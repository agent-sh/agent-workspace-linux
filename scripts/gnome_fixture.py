#!/usr/bin/env python3
"""Native GTK fixture records client-side input and allocation evidence."""
import gi
import json
import os
from pathlib import Path
import sys
gi.require_version('Gtk', '4.0')
from gi.repository import Gtk, GLib
output = Path(sys.argv[1])
state = {'clicks': 0, 'text': '', 'scrolls': [], 'motions': []}
app = Gtk.Application(application_id='org.agent_sh.GnomeFixture')
def save():
    state.update(scale=window.get_scale_factor(), width=window.get_width(), height=window.get_height())
    ok, bounds = button.compute_bounds(window)
    if ok: state['button_rect']=[bounds.get_x(),bounds.get_y(),bounds.get_width(),bounds.get_height()]
    output.write_text(json.dumps(state))
def activate(app):
    global window, button
    window = Gtk.ApplicationWindow(application=app, title='Native GNOME fixture')
    window.set_default_size(480, 320)
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
    window.set_child(box)
    entry = Gtk.Entry()
    entry.set_placeholder_text('Native text input')
    entry.connect('changed', lambda e: (state.update(text=e.get_text()), save()))
    box.append(entry)
    button = Gtk.Button(label='Native click target')
    button.set_size_request(-1, 80)
    def clicked(_):
        state['clicks'] += 1
        save()
    button.connect('clicked', clicked)
    box.append(button)
    scroll = Gtk.EventControllerScroll.new(Gtk.EventControllerScrollFlags.BOTH_AXES)
    def scrolled(_c, dx, dy):
        state['scrolls'].append([dx, dy])
        save()
        return True
    scroll.connect('scroll', scrolled)
    window.add_controller(scroll)
    motion = Gtk.EventControllerMotion.new()
    def moved(_c, x, y):
        state['motions'] = [x, y]
        save()
    motion.connect('motion', moved)
    window.add_controller(motion)
    window.present()
    entry.grab_focus()
    GLib.timeout_add(200, lambda: (save(), True)[1])
app.connect('activate', activate)
app.run([])
