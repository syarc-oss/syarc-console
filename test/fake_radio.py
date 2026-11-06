"""Fake MeshCore companion radio for testing the Rust direct-radio client.

Speaks the companion serial/TCP protocol: frames 0x3c/0x3e + LE16 length.
"""
import asyncio
import struct

APP_START = 1
SEND_TXT = 2
SEND_CHAN = 3
GET_CONTACTS = 4
SET_TIME = 6
GET_CHANNEL = 0x1F
SYNC_NEXT = 0x0A

R_OK = 0
R_ERROR = 1
R_CONTACT_START = 2
R_CONTACT = 3
R_CONTACT_END = 4
R_SELF_INFO = 5
R_MSG_SENT = 6
R_CONTACT_MSG_V3 = 16
R_CHANNEL_INFO = 18
R_NO_MORE = 10


def frame(payload: bytes) -> bytes:
    return b"\x3e" + struct.pack("<H", len(payload)) + payload


class Radio(asyncio.Protocol):
    def connection_made(self, transport):
        self.transport = transport
        self.buf = b""
        if self not in RADIOS:
            RADIOS.append(self)
        print("fake radio: client connected")

    def data_received(self, data):
        self.buf += data
        while True:
            if len(self.buf) < 3 or self.buf[0] != 0x3C:
                idx = self.buf.find(b"\x3c")
                if idx < 0:
                    self.buf = b""
                    return
                self.buf = self.buf[idx:]
            (ln,) = struct.unpack("<H", self.buf[1:3])
            if len(self.buf) < 3 + ln:
                return
            payload = self.buf[3 : 3 + ln]
            self.buf = self.buf[3 + ln :]
            self.handle(payload)

    def handle(self, p):
        cmd = p[0]
        if cmd == APP_START:
            body = bytes([1, 10, 22]) + bytes(range(32)) + struct.pack("<ii", 47200000, -12200000) \
                + bytes([0, 0, 0, 0]) + struct.pack("<II", 906000000, 250000) \
                + bytes([9, 7]) + b"FakeRadio"
            self.reply(bytes([R_SELF_INFO]) + body)
            # after handshake, push an unsolicited contact DM and a channel msg
            asyncio.get_event_loop().call_later(0.5, self.push_contact_msg)
            asyncio.get_event_loop().call_later(0.8, self.push_chan_msg)
        elif cmd == SET_TIME:
            self.reply(bytes([R_OK]) + struct.pack("<I", 12345))
        elif cmd == GET_CHANNEL:
            idx = p[1]
            name = f"Test{idx}".encode().ljust(32, b"\x00")
            self.reply(bytes([R_CHANNEL_INFO, idx]) + name + bytes(16))
        elif cmd == GET_CONTACTS:
            self.reply(bytes([R_CONTACT_START]) + struct.pack("<I", 1))
            c = bytes(32) + bytes([1, 0, 0]) + bytes(64) + b"alice".ljust(32, b"\x00") \
                + struct.pack("<iii", 1000, 47200000, -12200000) + struct.pack("<I", 0)
            self.reply(bytes([R_CONTACT]) + c)
            self.reply(bytes([R_CONTACT_END]) + struct.pack("<I", 1000))
        elif cmd == 0x16:  # DEVICE_QUERY
            b = bytes([10, 100, 8]) + struct.pack("<I", 123456) \
                + b"2026-10-01".ljust(12, b"\x00") \
                + b"Heltec V3".ljust(40, b"\x00") + b"1.10.0".ljust(20, b"\x00") \
                + bytes([0, 0])
            self.reply(bytes([R_DEVICE_INFO_BACKUP := 13]) + b)
        elif cmd == 0x08:  # SET_ADVERT_NAME
            print("fake radio: SET_ADVERT_NAME ->", p[1:33].decode(errors="replace").rstrip("\x00"))
            self.reply(bytes([R_OK]))
        elif cmd == 0x0E:  # SET_ADVERT_LATLON
            lat, lon = struct.unpack("<ii", p[1:9])
            print(f"fake radio: SET_ADVERT_LATLON -> {lat/1e6:.6f},{lon/1e6:.6f}")
            self.reply(bytes([R_OK]))
        elif cmd == 0x0C:  # SET_RADIO_TX_POWER
            print("fake radio: SET_TX_POWER ->", struct.unpack("<i", p[1:5])[0])
            self.reply(bytes([R_OK]))
        elif cmd == 0x26:  # SET_OTHER_PARAMS
            print("fake radio: SET_OTHER_PARAMS ->", list(p[1:5]))
            self.reply(bytes([R_OK]))
        elif cmd == 0x07:  # SEND_SELF_ADVERT
            print("fake radio: SEND_ADVERT flood=", p[1] if len(p) > 1 else 0)
            self.reply(bytes([R_OK]))
        elif cmd == 0x13:  # REBOOT
            print("fake radio: REBOOT")
            self.reply(bytes([R_OK]))
        elif cmd == SEND_TXT:
            print("fake radio: SEND_TXT ->", p[1:][:60])
            self.reply(bytes([R_MSG_SENT, 0]) + b"\xde\xad\xbe\xef" + struct.pack("<I", 4000))
        elif cmd == SEND_CHAN:
            print("fake radio: SEND_CHAN chan=", p[2], "->", p[7:][:60])
            self.reply(bytes([R_OK]))
        elif cmd == SYNC_NEXT:
            self.reply(bytes([R_NO_MORE]))
        else:
            print(f"fake radio: unhandled cmd {cmd}")
            self.reply(bytes([R_ERROR, 99]))

    def reply(self, payload):
        self.transport.write(frame(payload))

    def push_contact_msg(self):
        body = bytes([12]) + bytes(2) + bytes([0xAA] * 6) + bytes([255]) + bytes([0]) \
            + struct.pack("<I", 1700000000) + b"hello from alice over mesh"
        self.reply(bytes([R_CONTACT_MSG_V3]) + body)
        print("fake radio: pushed contact msg")

    def push_new_advert(self, name):
        key = bytes([0xBB]) * 32
        body = key + bytes([1, 0]) + bytes([0]) + bytes(64) \
            + name.encode().ljust(32, b"\x00")[:32] \
            + struct.pack("<IiiI", 1700000000, 47100000, -12260000, 1700000000)
        self.reply(bytes([0x8A]) + body)
        print(f"fake radio: pushed flood advert: {name!r}")

    def push_contact_dm(self, text):
        body = bytes([12]) + bytes(2) + bytes([0xAA] * 6) + bytes([255]) + bytes([0]) \
            + struct.pack("<I", int(asyncio.get_event_loop().time()) + 1700000000) + text.encode()
        self.reply(bytes([R_CONTACT_MSG_V3]) + body)
        print(f"fake radio: pushed contact dm: {text!r}")

    def push_chan_msg(self):
        self.push_chan_text(0, "channel chatter")

    def push_chan_text(self, idx, text):
        body = bytes([10]) + bytes(2) + bytes([idx]) + bytes([255]) + bytes([0]) \
            + struct.pack("<I", int(asyncio.get_event_loop().time()) + 1700000000) + text.encode()
        self.reply(bytes([17]) + body)
        print(f"fake radio: pushed chan {idx} msg: {text!r}")
        print("fake radio: pushed channel msg")


RADIOS = []


async def stdin_loop():
    """Feed messages from a command file (default /tmp/radio_cmds):
    'chan <idx> <text>' or 'dm <text>', one per line. Send commands with:
    echo 'chan 0 ? question' >> /tmp/radio_cmds"""
    import os
    path = os.environ.get("FAKE_RADIO_CMDS", "/tmp/radio_cmds")
    done = 0  # lines already processed
    while True:
        await asyncio.sleep(1)
        try:
            with open(path) as f:
                lines = f.read().splitlines()
        except FileNotFoundError:
            continue
        for line in lines[done:]:
            done += 1
            line = line.strip()
            if not line:
                continue
            if not RADIOS:
                print("fake radio: no client connected")
                continue
            try:
                if line.startswith("chan "):
                    idx, text = line[5:].split(" ", 1)
                    RADIOS[-1].push_chan_text(int(idx), text)
                elif line.startswith("dm "):
                    RADIOS[-1].push_contact_dm(line[3:])
                elif line.startswith("advert "):
                    RADIOS[-1].push_new_advert(line[7:])
                else:
                    print("usage: chan <idx> <text> | dm <text>")
            except Exception as e:
                print("fake radio: bad command:", e)


async def main():
    server = await asyncio.get_event_loop().create_server(Radio, "127.0.0.1", 14000)
    print("fake radio listening on 14000")
    asyncio.create_task(stdin_loop())
    async with server:
        await server.serve_forever()


asyncio.run(main())
