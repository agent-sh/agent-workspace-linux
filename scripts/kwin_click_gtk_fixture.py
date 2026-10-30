#!/usr/bin/env python3
"""GTK client for the KWin button-delivery regression."""
import json
from pathlib import Path
import sys
import gi
gi.require_version('Gtk', '3.0')
gi.require_version('Gdk', '3.0')
from gi.repository import Gdk, GLib, Gtk

output = Path(sys.argv[1])
state = {'events': [], 'activations': 0, 'targets': {}, 'ticks': 0, 'scrolls': 0}
window = Gtk.Window(title='KWin click regression')
window.set_decorated(False)
window.set_default_size(400, 220)
fixed = Gtk.Fixed()
window.add(fixed)
buttons = []

def save():
    for button in buttons:
        rect = button.get_allocation()
        state['targets'][button.get_label()] = [rect.x + rect.width // 2, rect.y + rect.height // 2]
    staging = output.with_suffix('.staging')
    staging.write_text(json.dumps(state))
    staging.replace(output)

def record(button, event, kind):
    state['events'].append({'target': button.get_label(), 'button': event.button, 'kind': kind})
    save()
    return False

def activated(_):
    state['activations'] += 1
    save()

def scrolled(_, event):
    state['scrolls'] += 1
    save()
    return True

for index, name in enumerate(['left', 'right']):
    button = Gtk.Button(label=name)
    button.set_size_request(140, 80)
    fixed.put(button, 30 + index * 200, 60)
    button.connect('button-press-event', record, 'press')
    button.connect('button-release-event', record, 'release')
    button.connect('clicked', activated)
    button.add_events(Gdk.EventMask.SCROLL_MASK | Gdk.EventMask.SMOOTH_SCROLL_MASK)
    button.connect('scroll-event', scrolled)
    buttons.append(button)

def tick():
    state['ticks'] += 1
    save()
    return True

GLib.timeout_add(50, tick)
window.connect('destroy', Gtk.main_quit)
window.show_all()
Gtk.main()
