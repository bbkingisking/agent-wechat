"""StatusNotifier watcher for the isolated native tray acceptance test."""
import json
import os
import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib

assert os.environ.get("TRAY_ICON_TEST_SESSION") == "1"
DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
name = dbus.service.BusName("org.kde.StatusNotifierWatcher", bus)
interface = "org.kde.StatusNotifierItem"
matches = []


def emit(value):
    print(json.dumps(value), flush=True)


def observe(destination, path):
    def status(value):
        properties = dbus.Interface(bus.get_object(destination, path), "org.freedesktop.DBus.Properties")
        actual = properties.Get(interface, "Status")
        emit({"event": "status", "value": str(value), "property": str(actual)})

    matches.append(bus.add_signal_receiver(status, signal_name="NewStatus", dbus_interface=interface,
                                           bus_name=destination, path=path))
    properties = dbus.Interface(bus.get_object(destination, path), "org.freedesktop.DBus.Properties")
    status(properties.Get(interface, "Status"))
    return False


class Watcher(dbus.service.Object):
    def __init__(self):
        super().__init__(bus, "/StatusNotifierWatcher")

    @dbus.service.method("org.kde.StatusNotifierWatcher", in_signature="s", out_signature="", sender_keyword="sender")
    def RegisterStatusNotifierItem(self, service, sender=None):
        # Return the registration reply before querying the app's GTK main loop.
        destination = sender if service.startswith("/") else str(service)
        path = str(service) if service.startswith("/") else "/StatusNotifierItem"
        GLib.idle_add(observe, destination, path)

    @dbus.service.method("org.kde.StatusNotifierWatcher", in_signature="s", out_signature="")
    def RegisterStatusNotifierHost(self, service):
        pass

    @dbus.service.method("org.freedesktop.DBus.Properties", in_signature="s", out_signature="a{sv}")
    def GetAll(self, requested):
        return {"IsStatusNotifierHostRegistered": dbus.Boolean(True),
                "ProtocolVersion": dbus.Int32(0),
                "RegisteredStatusNotifierItems": dbus.Array([], signature="s")}

    @dbus.service.method("org.freedesktop.DBus.Properties", in_signature="ss", out_signature="v")
    def Get(self, requested, property_name):
        return self.GetAll(requested)[property_name]


watcher = Watcher()
emit({"event": "ready"})
GLib.MainLoop().run()
