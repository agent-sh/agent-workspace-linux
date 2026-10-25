#!/usr/bin/env python3
"""Record what PRIMARY offers to an ordinary X11 client."""
import json
import sys
from pathlib import Path
import gi
gi.require_version('Gdk', '4.0')
gi.require_version('Gtk', '4.0')
from gi.repository import Gdk, GLib, Gtk
Gtk.init()
output = Path(sys.argv[1])
clipboard = Gdk.Display.get_default().get_primary_clipboard()
loop = GLib.MainLoop()
def done(source, result):
    try:
        state = {'text': source.read_text_finish(result)}
    except GLib.Error as error:
        state = {'text': None, 'error': error.message}
    output.write_text(json.dumps(state))
    loop.quit()
def read():
    clipboard.read_text_async(None, done)
    return GLib.SOURCE_REMOVE
GLib.timeout_add(300, read)
loop.run()
