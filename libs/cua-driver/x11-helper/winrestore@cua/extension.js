const Gio = imports.gi.Gio;
const GLib = imports.gi.GLib;

const IFACE = `<node><interface name="org.cua.WinRestore">
<method name="GetVersion"><arg type="u" direction="out" name="version"/></method>
<method name="Restore"><arg type="u" direction="in" name="pid"/><arg type="u" direction="in" name="xid"/><arg type="s" direction="in" name="title"/><arg type="b" direction="out" name="restored"/></method>
</interface></node>`;

function xWindowId(window) {
  if (!window || typeof window.get_description !== 'function') {
    return null;
  }
  const description = window.get_description();
  if (typeof description !== 'string' || !/^0x[0-9a-f]+$/i.test(description)) {
    return null;
  }
  const xid = Number.parseInt(description.slice(2), 16);
  return Number.isSafeInteger(xid) && xid >= 0 && xid <= 0xffffffff ? xid : null;
}

class WinRestoreExtension {
  enable() {
    this._impl = Gio.DBusExportedObject.wrapJSObject(IFACE, this);
    this._impl.export(Gio.DBus.session, '/org/cua/WinRestore');
    this._nameId = Gio.bus_own_name(
      Gio.BusType.SESSION,
      'org.cua.WinRestore',
      Gio.BusNameOwnerFlags.REPLACE,
      null,
      null,
      null
    );
  }

  disable() {
    if (this._impl) {
      this._impl.unexport();
      this._impl = null;
    }
    if (this._nameId) {
      Gio.bus_unown_name(this._nameId);
      this._nameId = 0;
    }
  }

  GetVersion() {
    return 2;
  }

  RestoreAsync([pid, xid, title], invocation) {
    const windows =
      typeof global.display.list_all_windows === 'function'
        ? global.display.list_all_windows()
        : global.get_window_actors().map((actor) => actor.meta_window);
    const matches = windows
      .filter(
        (window) =>
          window &&
          window.get_pid() === pid &&
          xWindowId(window) === xid &&
          window.get_title() === title
      );
    if (matches.length !== 1 || !matches[0].minimized) {
      invocation.return_value(new GLib.Variant('(b)', [false]));
      return;
    }
    const target = matches[0];
    const focusedBefore = global.display.focus_window;
    target.unminimize();
    GLib.timeout_add(GLib.PRIORITY_DEFAULT, 100, () => {
      invocation.return_value(
        new GLib.Variant('(b)', [!target.minimized && global.display.focus_window === focusedBefore])
      );
      return GLib.SOURCE_REMOVE;
    });
  }
}

function init() {
  return new WinRestoreExtension();
}
