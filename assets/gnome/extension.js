import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import St from 'gi://St';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const XML = `<node><interface name="org.agent_sh.Workspace.Gnome"><method name="Command"><arg type="s" direction="in"/><arg type="s" direction="out"/></method></interface></node>`;
const delay = () => new Promise(resolve => GLib.timeout_add(GLib.PRIORITY_DEFAULT, 30, () => { resolve(); return GLib.SOURCE_REMOVE; }));

export default class Workspace extends Extension {
    enable() {
        if (global.session_mode !== 'agent-workspace')
            throw new Error('This extension requires the private workspace session mode');
        this._controller = Number(GLib.getenv('AGENT_WORKSPACE_CONTROLLER_PID'));
        if (!Number.isSafeInteger(this._controller) || this._controller < 1)
            throw new Error('Missing workspace controller PID');
        this._seat = global.stage.context.get_backend().get_default_seat();
        this._pointer = this._seat.create_virtual_device(Clutter.InputDeviceType.POINTER_DEVICE);
        this._keyboard = this._seat.create_virtual_device(Clutter.InputDeviceType.KEYBOARD_DEVICE);
        Main.panel.hide();
        this._object = Gio.DBusExportedObject.wrapJSObject(XML, this);
        this._object.export(Gio.DBus.session, '/org/agent_sh/Workspace/Gnome');
        this._name = Gio.bus_own_name_on_connection(Gio.DBus.session, 'org.agent_sh.Workspace.Gnome', Gio.BusNameOwnerFlags.NONE, null, null);
    }
    disable() {
        this._object?.unexport();
        if (this._name) Gio.bus_unown_name(this._name);
        this._pointer = null;
        this._keyboard = null;
    }
    async CommandAsync([serialized], invocation) {
        try {
            // A sandboxed app may use the private bus but cannot act as its
            // daemon. Never expose Eval, script loading or arbitrary file IO.
            const reply = Gio.DBus.session.call_sync('org.freedesktop.DBus', '/org/freedesktop/DBus',
                'org.freedesktop.DBus', 'GetConnectionUnixProcessID',
                new GLib.Variant('(s)', [invocation.get_sender()]), new GLib.VariantType('(u)'),
                Gio.DBusCallFlags.NONE, 2000, null);
            if (reply.deep_unpack()[0] !== this._controller)
                throw new Error('Only the workspace daemon may control this session');
            const value = await this._command(JSON.parse(serialized));
            invocation.return_value(new GLib.Variant('(s)', [JSON.stringify({ok: true, value})]));
        } catch (error) {
            invocation.return_value(new GLib.Variant('(s)', [JSON.stringify({ok: false, error: String(error)})]));
        }
    }
    _windows() { return global.get_window_actors().map(a => a.meta_window).filter(w => !w.is_override_redirect() && w.get_frame_rect().width > 0); }
    _window(id) {
        const w = this._windows().find(w => `gnome:${w.get_stable_sequence()}` === id);
        if (!w) throw new Error(`Window no longer exists: ${id}`);
        return w;
    }
    _scale() { return global.display.get_monitor_scale(0); }
    _describe(w) {
        const r = w.get_frame_rect(), s = this._scale();
        return {id: `gnome:${w.get_stable_sequence()}`, title: w.get_title() ?? '',
            wm_class: w.get_wm_class(), wm_instance: w.get_wm_class_instance(), pid: w.get_pid() > 0 ? w.get_pid() : null,
            app_id: w.get_gtk_application_id(), visible: !w.minimized,
            geometry: {x: Math.round(r.x*s), y: Math.round(r.y*s), width: Math.round(r.width*s), height: Math.round(r.height*s), screen: 0}};
    }
    async _command(q) {
        const s = this._scale(), time = () => GLib.get_monotonic_time();
        const pointer = (x,y) => this._pointer.notify_absolute_motion(time(), x/s, y/s);
        const button = (b, pressed) => this._pointer.notify_button(time(), b, pressed ? Clutter.ButtonState.PRESSED : Clutter.ButtonState.RELEASED);
        const key = (k, pressed) => this._keyboard.notify_keyval(time(), k, pressed ? Clutter.KeyState.PRESSED : Clutter.KeyState.RELEASED);
        switch (q.op) {
        case 'probe': return {ready: !Main.layoutManager._startingUp, display: GLib.getenv('DISPLAY'), xauthority: GLib.getenv('XAUTHORITY'), scale: s, width: Math.round(global.stage.width*s), height: Math.round(global.stage.height*s), monitors: global.display.get_n_monitors(), session_mode: global.session_mode, has_run_dialog: Main.sessionMode.hasRunDialog, has_overview: Main.sessionMode.hasOverview};
        case 'windows': return this._windows().filter(w => q.hidden || !w.minimized).map(w => this._describe(w));
        case 'active': return global.display.focus_window ? this._describe(global.display.focus_window) : null;
        case 'window': return this._describe(this._window(q.id));
        case 'pointer': { const [x,y] = global.get_pointer(); return {x: Math.round(x*s), y: Math.round(y*s), screen: 0, window_id: global.display.focus_window ? `gnome:${global.display.focus_window.get_stable_sequence()}` : null}; }
        case 'focus': { const w=this._window(q.id); w.unminimize(); w.activate(global.get_current_time()); break; }
        case 'close': this._window(q.id).delete(global.get_current_time()); break;
        case 'move': this._window(q.id).move_frame(true, Math.round(q.x/s), Math.round(q.y/s)); break;
        case 'resize': { const w=this._window(q.id), r=w.get_frame_rect(); w.unmaximize(); w.move_resize_frame(true, r.x, r.y, Math.round(q.width/s), Math.round(q.height/s)); break; }
        case 'raise': this._window(q.id).raise(); break;
        case 'minimize': this._window(q.id).minimize(); break;
        case 'show': this._window(q.id).unminimize(); break;
        case 'motion': pointer(q.x,q.y); break;
        case 'click': pointer(q.x,q.y); await delay(); for(let i=0;i<q.count;i++) { button(q.button,true); button(q.button,false); } break;
        case 'drag': pointer(q.fx,q.fy); await delay(); button(q.button,true); for(let i=1;i<=12;i++) { pointer(q.fx+(q.tx-q.fx)*i/12,q.fy+(q.ty-q.fy)*i/12); await delay(); } button(q.button,false); break;
        case 'scroll': pointer(q.x,q.y); await delay(); for(let i=0;i<q.amount;i++) this._pointer.notify_discrete_scroll(time(), q.direction, Clutter.ScrollSource.WHEEL); break;
        case 'keys': for(const k of q.keys) key(k,true); for(const k of [...q.keys].reverse()) key(k,false); break;
        case 'text': {
            const window = global.display.focus_window;
            if (!window) throw new Error('No focused window for text input');
            if (window.get_client_type() === Meta.WindowClientType.X11)
                return {x11: true};
            for (const part of q.text.split(/([\n\r\t])/)) {
                if (!part) continue;
                if (/^[\n\r\t]$/.test(part)) {
                    const k = part === '\t' ? Clutter.KEY_Tab : Clutter.KEY_Return;
                    key(k, true); key(k, false); await delay();
                } else if (Main.inputMethod.currentFocus) {
                    Main.inputMethod.commit(part);
                } else if (/^[\x20-\x7e]*$/.test(part)) {
                    for (const c of part) { key(c.codePointAt(0), true); key(c.codePointAt(0), false); }
                } else {
                    throw new Error('Native Unicode input requires a focused Wayland text-input field');
                }
            }
            await delay();
            return {x11: false};
        }
        case 'clipboard-set': St.Clipboard.get_default().set_text(St.ClipboardType.CLIPBOARD,q.text); break;
        case 'clipboard-get': return await new Promise(resolve => St.Clipboard.get_default().get_text(St.ClipboardType.CLIPBOARD, (_c,text) => resolve(text ?? '')));
        case 'screenshot': {
            const stream = Gio.MemoryOutputStream.new_resizable();
            const screenshot = new Shell.Screenshot();
            const old = global.display.focus_window;
            let window = null, minimized = false;
            try {
                if(q.id) { window=this._window(q.id); minimized=window.minimized; window.unminimize(); window.activate(global.get_current_time()); await delay(); }
                if(window) await screenshot.screenshot_window(true,true,stream);
                else await screenshot.screenshot(true,stream);
                stream.close(null);
                const png = Array.from(stream.steal_as_bytes().get_data());
                if (png.length === 0)
                    throw new Error('GNOME PNG encoding returned no data; check the private bwrap helper');
                return {png};
            } finally {
                if(window && minimized) window.minimize();
                if(window && old && old !== window) old.activate(global.get_current_time());
            }
        }
        default: throw new Error(`Unknown fixed workspace operation: ${q.op}`);
        }
        await delay();
        return true;
    }
}
