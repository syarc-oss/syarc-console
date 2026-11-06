# MeshCore Radio Setup

How to connect the Syarc Console's MeshCore client board (e.g. the WIO
tracker) to the console. The radio link runs **inside the console
process** — there is no separate daemon to install.

## Overview

The console speaks the companion-radio serial protocol directly. At
startup (if `mesh_radio_enabled=1` or `MESH_RADIO_ENABLED` is set) it
starts a client that keeps a persistent session with the board:
handshake, contacts directory sync, channel list, message send/receive.
Status is visible at **Messages → Mesh Bridge → MeshCore** in the
Transport card, which shows the transport in use and the live
connection detail (what it is currently trying to open and why it may
be offline).

Four transports are available, selected by settings/env vars:

| Transport | When it is used | Configuration |
|---|---|---|
| USB / wired serial | default when a device path is given | `mesh_radio_host=/dev/ttyACM0` |
| Bluetooth, console binds | `MESH_RADIO_BLUETOOTH=<MAC>` | console runs `rfcomm bind` |
| Bluetooth, pre-bound | `MESH_RADIO_BLUETOOTH=1` | you bind `/dev/rfcommN` yourself |
| TCP | host:port instead of a path | for the standalone/broker setups |

Settings can come from the `settings` table (DB) **or** environment
variables; the env var is the fallback when the key is absent.

## 1. USB (wired) — the default

1. Plug the board in over USB. It enumerates as a CDC serial device,
   usually `/dev/ttyACM0` (or `/dev/ttyUSB*` for some adapters):
   ```bash
   dmesg | tail        # look for "ttyACM0 now attached"
   ls -l /dev/ttyACM*
   ```
2. Enable the radio and point it at the device:
   ```bash
   # environment (e.g. in the systemd unit or your shell):
   MESH_RADIO_ENABLED=1
   MESH_RADIO_HOST=/dev/ttyACM0
   ```
   Any value starting with `/` is treated as a serial device; anything
   else is treated as `host[:port]` TCP.
3. Restart the console. The log should show:
   ```
   mesh radio (direct MeshCore) enabled; connecting to /dev/ttyACM0
   mesh radio connected: <node name> via /dev/ttyACM0
   ```

### USB permissions

The console user needs access to the serial device. Prefer a udev rule
so it works across replugs:

```bash
# /etc/udev/rules.d/99-meshcore.rules
SUBSYSTEM=="tty", ATTRS{idVendor}=="2886", ATTRS{idProduct}=="802d", \
  MODE="0660", GROUP="dialout", SYMLINK+="meshcore"
```
(`2886:802d` is the WIO tracker's USB CDC id; adjust with
`lsusb`. With the symlink you can set `MESH_RADIO_HOST=/dev/meshcore`
for a stable path.) Then:
```bash
sudo usermod -aG dialout <console-user>
sudo udevadm control --reload && sudo udevadm trigger
```
Quick alternative without udev: `sudo chmod 666 /dev/ttyACM0`
(does not survive replug/reboot).

## 2. Bluetooth — console binds the device

The board's Bluetooth side exposes a classic serial-port profile
(SPP). Pair once, then let the console create the RFCOMM serial node
whenever it starts.

1. Pair and trust the board (one-time, interactive):
   ```bash
   bluetoothctl
   [bluetooth]# power on
   [bluetooth]# agent on
   [bluetooth]# default-agent
   [bluetooth]# scan on
   #    wait for the tracker to appear, note its MAC AA:BB:CC:DD:EE:FF
   [bluetooth]# pair AA:BB:CC:DD:EE:FF
   [bluetooth]# trust AA:BB:CC:DD:EE:FF        # allows auto-reconnect
   [bluetooth]# exit
   ```
2. Configure the console:
   ```bash
   MESH_RADIO_ENABLED=1
   MESH_RADIO_BLUETOOTH=AA:BB:CC:DD:EE:FF
   ```
   On startup (and on reconnect after dropouts) the console runs
   `rfcomm bind 0 <MAC>` (creating `/dev/rfcomm0`), waits up to ~5s
   for the node, then opens it at 115200 baud.
3. The bind requires bluez-utils plus permission to run `rfcomm bind`
   (normally root/CAP_NET_ADMIN). Options:
   - run the console as root (not recommended),
   - a sudoers NOPASSWD entry for `rfcomm`,
   - or use the pre-bound style below, which needs no privileges.

Requires: `bluez` and `bluez-utils` (provides `rfcomm`).

## 3. Bluetooth — pre-bound rfcomm node (no privileges)

Do the binding yourself (manually or via udev/systemd helper) and tell
the console to just open the node:

```bash
# bind the trusted device to /dev/rfcomm0
sudo rfcomm bind 0 AA:BB:CC:DD:EE:FF

# console configuration:
MESH_RADIO_ENABLED=1
MESH_RADIO_BLUETOOTH=1        # "1" = use the existing /dev/rfcommN
```

To persist the binding across boots, a small systemd unit:

```ini
# /etc/systemd/system/meshcore-bind.service
[Unit]
Description=Bind MeshCore tracker to rfcomm0
After=bluetooth.target
[Service]
Type=oneshot
ExecStart=/usr/bin/rfcomm bind 0 AA:BB:CC:DD:EE:FF
RemainAfterExit=yes
[Install]
WantedBy=multi-user.target
```

Both Bluetooth styles honour `MESH_RADIO_RFCOMM_DEV` (default
`/dev/rfcomm0`) if you need a different rfcomm index.

Bluetooth setup for a laptop/WIO tracker typically looks like:

```bash
MESH_RADIO_ENABLED=1
MESH_RADIO_BLUETOOTH=1
MESH_RADIO_RFCOMM_DEV=/dev/rfcomm0
```

after the one-time `bluetoothctl` pair/trust and `rfcomm bind`.

## 4. TCP

`MESH_RADIO_HOST=host[:port]` (default port 4000) connects to a remote
serial-TCP bridge (e.g. `socat PTY,link=/dev/ttyACM0,raw TCP-LISTEN:4000`
on the machine that has the board). Useful when the board hangs off
another box — including the standalone USB-SSD console acting as the
radio server for other instances.

## Checking status and troubleshooting

- **UI**: Messages → Mesh Bridge → MeshCore. The Transport card shows
  the transport (`MeshCore radio (direct)`) and a monospace detail line
  such as `connecting bluetooth AA:BB:.. (/dev/rfcomm0)` or
  `connected`. The node Settings page shows the same detail when the
  radio is offline.
- **Logs**: `journalctl -u syarc-console -f | grep -i mesh` (or the
  service's log). Expect `mesh radio connected: <name>` and periodic
  session-ended/retry lines.
- **Only one process can hold the radio.** A test instance on another
  port will show `connecting tcp /dev/ttyACM0` forever if the
  production instance already owns the device.
- **Empty Public channel**: no connected radio means nothing is
  ingested — fix the transport first.
- **rfcomm node never appears**: check `bluetoothctl info <MAC>` shows
  `Trusted: yes`, `rfcomm` exists (`which rfcomm`), and bluetoothd is
  running.
- **Everything wired but still offline**: unplug/replug and watch
  `dmesg`; confirm the user is in `dialout`; confirm no other process
  (`fuser /dev/ttyACM0`) holds the port.

## Reference

| Key (settings table) | Env var | Meaning |
|---|---|---|
| `mesh_radio_enabled=1` | `MESH_RADIO_ENABLED` | enable the built-in radio client |
| `mesh_radio_host` | `MESH_RADIO_HOST` | serial device path or TCP host:port |
| `mesh_radio_bluetooth` | `MESH_RADIO_BLUETOOTH` | MAC (console binds), `1` (pre-bound), empty (wired) |
| — | `MESH_RADIO_RFCOMM_DEV` | rfcomm node to open (default `/dev/rfcomm0`) |

The mesh *broker* is a separate, optional path used only when no radio
is attached; with any of the setups above it is not involved.
