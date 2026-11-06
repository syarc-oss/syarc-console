"""Serial-to-TCP bridge for the MeshCore companion radio.

Exposes /dev/ttyACM0 (LoneBear) on TCP 127.0.0.1:14000 so the Rust
console's direct-radio client can attach to it. One TCP client at a
time; the serial port is only opened while a client is connected.

Usage: python3 test/serial_bridge.py [serial_dev] [tcp_port]
"""
import asyncio
import sys

import serial

SERIAL_DEV = sys.argv[1] if len(sys.argv) > 1 else "/dev/ttyACM0"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 14000


async def handle(client_r, client_w):
    print(f"bridge: client connected, opening {SERIAL_DEV}", flush=True)
    try:
        ser = serial.Serial(SERIAL_DEV, 115200, timeout=0)
    except Exception as e:
        print(f"bridge: cannot open serial: {e}", flush=True)
        client_w.close()
        return

    async def tcp_to_serial():
        try:
            while True:
                data = await client_r.read(4096)
                if not data:
                    break
                ser.write(data)
                ser.flush()
        except Exception:
            pass

    async def serial_to_tcp():
        try:
            while True:
                n = ser.in_waiting
                if n:
                    client_w.write(ser.read(n))
                    await client_w.drain()
                else:
                    await asyncio.sleep(0.02)
        except Exception:
            pass

    await asyncio.wait({asyncio.create_task(tcp_to_serial()),
                        asyncio.create_task(serial_to_tcp())},
                       return_when=asyncio.FIRST_COMPLETED)
    ser.close()
    client_w.close()
    print("bridge: client disconnected, serial closed", flush=True)


async def main():
    server = await asyncio.start_server(handle, "127.0.0.1", PORT)
    print(f"bridge: listening on 127.0.0.1:{PORT} -> {SERIAL_DEV}", flush=True)
    async with server:
        await server.serve_forever()


asyncio.run(main())
