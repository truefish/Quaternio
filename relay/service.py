"""Private invitation signaling and short-lived Coturn credentials."""
import base64
import hashlib
import hmac
import json
import os
import re
import secrets
import threading
import time
import socket
import queue
import ipaddress
from collections import deque
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

KEY = os.environ["QUATERNIO_RELAY_KEY"]
SECRET = os.environ["QUATERNIO_TURN_SECRET"]
TURN_URLS = os.environ["QUATERNIO_TURN_URLS"].split(",")
if len(KEY) < 32 or len(SECRET) < 32:
    raise RuntimeError("Private relay credentials must contain at least 32 characters")
ROOMS = {}
LOCK = threading.Lock()
LIMIT = 768 * 1024
TUNNELS = {}
TUNNEL_LOCK = threading.Lock()
PUBLIC = os.environ.get("QUATERNIO_PUBLIC", "0") == "1"
HTTPS_BUDGET = min(256, max(1, int(os.environ.get("QUATERNIO_HTTPS_GIB", "64")))) * 1024 * 1024 * 1024
ROOM_BUDGET = 16 * 1024 * 1024
PUBLIC_SESSIONS = {}
RATES = {}
RATE_LOCK = threading.Lock()
TRAFFIC = {}
TRAFFIC_LOCK = threading.Lock()


def allowed(key, maximum, window):
    now = time.monotonic()
    with RATE_LOCK:
        if len(RATES) >= 4096:
            for entry, (_, expiry) in list(RATES.items()):
                if expiry <= now:
                    del RATES[entry]
            if key not in RATES and len(RATES) >= 4096:
                return False
        count, expiry = RATES.get(key, (0, now + window))
        if expiry <= now:
            count, expiry = 0, now + window
        if count >= maximum:
            return False
        RATES[key] = (count + 1, expiry)
        return True


def charge(token, amount):
    if not PUBLIC:
        return True
    now = int(time.time())
    expiry = int(token.split(":", 1)[0])
    with TRAFFIC_LOCK:
        for key, (_, deadline) in list(TRAFFIC.items()):
            if deadline <= now:
                del TRAFFIC[key]
        used, _ = TRAFFIC.get(token, (0, expiry))
        if used + amount > HTTPS_BUDGET or len(TRAFFIC) >= 512 and token not in TRAFFIC:
            return False
        TRAFFIC[token] = (used + amount, expiry)
        return True


class PacketQueue:
    """Bound both bytes and packets while absorbing Internet relay bursts."""
    def __init__(self):
        self.items = deque()
        self.bytes = 0
        self.ready = threading.Condition()

    def put_nowait(self, packet):
        with self.ready:
            if len(self.items) >= 512 or self.bytes + len(packet) > 512 * 1024:
                raise queue.Full
            self.items.append(packet)
            self.bytes += len(packet)
            self.ready.notify()

    def get(self, timeout=0):
        with self.ready:
            if not self.ready.wait_for(lambda: bool(self.items), timeout):
                raise queue.Empty
            packet = self.items.popleft()
            self.bytes -= len(packet)
            return packet

    def get_nowait(self):
        return self.get()


class Tunnel:
    def __init__(self, token):
        self.token = token
        self.used = time.monotonic()
        self.closed = threading.Event()
        self.packets = PacketQueue()
        self.reader = threading.Lock()
        self.socket = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        # Destination is operator configuration, never supplied by a client.
        self.socket.connect(("127.0.0.1", int(os.environ.get("QUATERNIO_TURN_LOCAL_PORT", "3478"))))
        self.socket.settimeout(.2)
        threading.Thread(target=self.receive, daemon=True).start()

    def receive(self):
        try:
            while not self.closed.is_set() and time.monotonic() - self.used < 90:
                try:
                    packet = self.socket.recv(32768)
                    self.packets.put_nowait(base64.b64encode(packet).decode())
                except socket.timeout:
                    pass
                except queue.Full:
                    # SCTP retransmits; never grow a queue for a stalled client.
                    pass
        except OSError:
            pass
        finally:
            self.close()

    def close(self):
        self.closed.set()
        self.socket.close()


def tunnel_token(username):
    signature = hmac.new(SECRET.encode(), ("https-tunnel:" + username).encode(), hashlib.sha256).hexdigest()
    return username + ":" + signature


def valid_tunnel_token(token):
    try:
        username, signature = token.rsplit(":", 1)
        expiry = int(username.split(":", 1)[0])
        return time.time() < expiry <= time.time() + 86460 and secrets.compare_digest(token, tunnel_token(username))
    except (ValueError, TypeError):
        return False


def credentials(ttl=86400):
    username = f"{int(time.time()) + ttl}:{secrets.token_hex(12)}"
    password = base64.b64encode(hmac.new(SECRET.encode(), username.encode(), hashlib.sha1).digest()).decode()
    return {"urls": TURN_URLS, "username": username, "password": password, "tunnel_token": tunnel_token(username)}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def setup(self):
        super().setup()
        self.connection.settimeout(10)
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    def reply(self, status, value):
        if status >= 400:
            self.close_connection = True
        data = json.dumps(value, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        if self.close_connection:
            self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(data)

    def token(self, expected):
        value = self.headers.get("Authorization", "")
        return len(value) < 4096 and secrets.compare_digest(value, "Bearer " + expected)

    def body(self):
        if self.headers.get("Transfer-Encoding"):
            raise ValueError("Chunked requests are not accepted")
        length = int(self.headers.get("Content-Length", "0"))
        if not 0 < length <= LIMIT:
            raise ValueError("Invalid request size")
        value = json.loads(self.rfile.read(length))
        if not isinstance(value, dict):
            raise ValueError("Expected object")
        return value

    def identity(self):
        address = self.client_address[0]
        if ipaddress.ip_address(address).is_loopback:
            forwarded = self.headers.get("X-Forwarded-For", "").split(",")[-1].strip()
            try:
                address = str(ipaddress.ip_address(forwarded))
            except ValueError:
                pass
        return address

    def host_identity(self):
        if self.token(KEY):
            return "owner"
        value = self.headers.get("Authorization", "").removeprefix("Bearer ")
        with LOCK:
            session = PUBLIC_SESSIONS.get(value) if PUBLIC else None
            if session and session["expires"] > time.monotonic():
                return value
        return None

    def public_session(self):
        if not PUBLIC:
            return self.reply(404, {"error": "Public sessions are disabled"})
        if self.headers.get("Content-Length", "0") != "0":
            self.body()
        if not allowed(("issue", self.identity()), 4, 3600) or not allowed(("issue-global",), 24, 3600):
            return self.reply(429, {"error": "Public service is busy; try direct/manual connection"})
        with LOCK:
            now = time.monotonic()
            for token, session in list(PUBLIC_SESSIONS.items()):
                if session["expires"] <= now:
                    del PUBLIC_SESSIONS[token]
            if len(PUBLIC_SESSIONS) >= 32:
                result = (429, {"error": "Public session capacity reached"})
            else:
                token = secrets.token_hex(32)
                relay = credentials(14400)
                PUBLIC_SESSIONS[token] = {"expires": now + 14400, "relay": relay}
                result = (201, {**relay, "access_token": token})
        return self.reply(*result)

    def websocket(self, key, tunnel, token):
        if self.command != "GET" or self.headers.get("Upgrade", "").lower() != "websocket" or "upgrade" not in self.headers.get("Connection", "").lower() or self.headers.get("Sec-WebSocket-Version") != "13":
            return self.reply(400, {"error": "WebSocket upgrade required"})
        try:
            nonce = self.headers.get("Sec-WebSocket-Key", "")
            if len(base64.b64decode(nonce, validate=True)) != 16:
                raise ValueError("Invalid WebSocket key")
        except (ValueError, TypeError):
            return self.reply(400, {"error": "Invalid WebSocket key"})
        if not tunnel.reader.acquire(blocking=False):
            return self.reply(409, {"error": "A relay reader is already active"})
        writing = threading.Lock()
        def frame(opcode, payload):
            size = len(payload)
            header = bytes([0x80 | opcode, size]) if size < 126 else bytes([0x80 | opcode, 126]) + size.to_bytes(2, "big")
            with writing:
                self.wfile.write(header + payload)
        def exact(size):
            data = self.rfile.read(size)
            if len(data) != size:
                raise EOFError()
            return data
        def send():
            try:
                while not tunnel.closed.is_set():
                    try:
                        packet = base64.b64decode(tunnel.packets.get(timeout=.2))
                    except queue.Empty:
                        continue
                    if not valid_tunnel_token(token) or not charge(token, len(packet)):
                        break
                    frame(2, packet)
            except OSError:
                pass
            finally:
                tunnel.close()
        writer = None
        try:
            self.connection.settimeout(90)
            self.send_response(101)
            self.send_header("Upgrade", "websocket")
            self.send_header("Connection", "Upgrade")
            accept = hashlib.sha1((nonce + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode(), usedforsecurity=False).digest()
            self.send_header("Sec-WebSocket-Accept", base64.b64encode(accept).decode())
            self.end_headers()
            writer = threading.Thread(target=send, daemon=True)
            writer.start()
            while not tunnel.closed.is_set():
                first, second = exact(2)
                opcode, size = first & 15, second & 127
                if first & 0x70 or not first & 0x80 or not second & 0x80 or opcode not in (2, 8, 9, 10):
                    frame(8, (1002).to_bytes(2, "big")); break
                if size == 126:
                    size = int.from_bytes(exact(2), "big")
                elif size == 127:
                    size = int.from_bytes(exact(8), "big")
                if size > 32768 or opcode >= 8 and size > 125 or not allowed(("ws-frames", key), 2000, 1):
                    frame(8, (1009).to_bytes(2, "big")); break
                mask = exact(4)
                packet = bytes(byte ^ mask[index % 4] for index, byte in enumerate(exact(size)))
                tunnel.used = time.monotonic()
                if opcode == 8:
                    frame(8, packet); break
                if not valid_tunnel_token(token) or not charge(token, len(packet)):
                    frame(8, (1008).to_bytes(2, "big")); break
                if opcode == 9:
                    frame(10, packet)
                elif opcode == 2:
                    if len(packet) < 4:
                        frame(8, (1002).to_bytes(2, "big")); break
                    tunnel.socket.send(packet)
        except (OSError, EOFError, ValueError):
            pass
        finally:
            self.close_connection = True
            tunnel.close()
            tunnel.reader.release()
            if writer:
                writer.join(timeout=.3)
            with TUNNEL_LOCK:
                if TUNNELS.get(key) is tunnel:
                    del TUNNELS[key]

    def tunnel_request(self):
        token = self.headers.get("Authorization", "").removeprefix("Bearer ")
        if len(token) > 512 or not valid_tunnel_token(token):
            return self.reply(401, {"error": "Unauthorized"})
        if self.path == "/v1/tunnels" and self.command == "POST" and self.headers.get("Content-Length", "0") != "0":
            self.body()
        with TUNNEL_LOCK:
            for key, tunnel in list(TUNNELS.items()):
                if tunnel.closed.is_set():
                    del TUNNELS[key]
            if self.path == "/v1/tunnels" and self.command == "POST":
                if len(TUNNELS) >= 32 or PUBLIC and sum(t.token == token for t in TUNNELS.values()) >= 8:
                    result = (429, {"error": "Relay is busy"})
                else:
                    key = secrets.token_hex(16)
                    TUNNELS[key] = Tunnel(token)
                    result = (201, {"id": key})
                tunnel = None
            else:
                match = re.fullmatch(r"/v1/tunnels/([a-f0-9]{32})(/ws)?", self.path)
                tunnel = TUNNELS.get(match[1]) if match else None
                result = (404, {"error": "Relay connection expired"})
        if tunnel is None:
            return self.reply(*result)
        if not secrets.compare_digest(tunnel.token, token):
            return self.reply(401, {"error": "Unauthorized"})
        tunnel.used = time.monotonic()
        if self.path.endswith("/ws"):
            return self.websocket(match[1], tunnel, token)
        if self.command == "DELETE":
            tunnel.close()
            return self.reply(200, {"ok": True})
        if self.command == "POST":
            packets = self.body().get("packets")
            if not isinstance(packets, list) or not 0 < len(packets) <= 64:
                raise ValueError("Invalid packets")
            decoded = []
            for packet in packets:
                if not isinstance(packet, str) or len(packet) > 44000:
                    raise ValueError("Invalid packet")
                data = base64.b64decode(packet, validate=True)
                if not 4 <= len(data) <= 32768:
                    raise ValueError("Invalid packet")
                decoded.append(data)
            if not charge(token, sum(map(len, decoded))):
                return self.reply(429, {"error": "Public relay transfer limit reached"})
            for data in decoded:
                tunnel.socket.send(data)
            return self.reply(200, {"ok": True})
        if self.command == "GET":
            if not tunnel.reader.acquire(blocking=False):
                return self.reply(409, {"error": "A relay reader is already active"})
            try:
                packets = []
                try:
                    packets.append(tunnel.packets.get(timeout=1))
                    while len(packets) < 16:
                        packets.append(tunnel.packets.get_nowait())
                except queue.Empty:
                    pass
            finally:
                tunnel.reader.release()
            if not charge(token, sum(len(packet) * 3 // 4 for packet in packets)):
                return self.reply(429, {"error": "Public relay transfer limit reached"})
            return self.reply(200, {"packets": packets})
        return self.reply(405, {"error": "Unsupported method"})

    def handle_request(self):
        try:
            if self.headers.get("Transfer-Encoding") or self.command in ("GET", "DELETE") and self.headers.get("Content-Length", "0") != "0":
                raise ValueError("Unexpected request body")
            if PUBLIC and not allowed(("requests", self.identity()), 2400, 60):
                return self.reply(429, {"error": "Request limit reached"})
            if self.path == "/health" and self.command == "GET":
                return self.reply(200, {"service": "quaternio-relay", "public": PUBLIC})
            if self.path == "/v1/public" and self.command == "POST":
                return self.public_session()
            if self.path.startswith("/v1/tunnels"):
                return self.tunnel_request()
            with LOCK:
                now = time.monotonic()
                for key in list(ROOMS):
                    if ROOMS[key]["expires"] < now:
                        del ROOMS[key]
            if self.path == "/v1/config" and self.command == "GET":
                identity = self.host_identity()
                if not identity:
                    return self.reply(401, {"error": "Unauthorized"})
                if identity == "owner":
                    return self.reply(200, credentials())
                with LOCK:
                    session = PUBLIC_SESSIONS.get(identity)
                    relay = session["relay"] if session else None
                return self.reply(200, relay) if relay else self.reply(401, {"error": "Public session expired"})
            if self.path == "/v1/rooms" and self.command == "POST":
                identity = self.host_identity()
                if not identity:
                    return self.reply(401, {"error": "Unauthorized"})
                offer = self.body().get("offer")
                if not isinstance(offer, str) or not offer.startswith("quaternio-peer-v1:") or len(offer) > 512 * 1024:
                    raise ValueError("Invalid invitation")
                with LOCK:
                    if sum(len(r["offer"]) + len(r["answer"] or "") for r in ROOMS.values()) + len(offer) > ROOM_BUDGET or len(ROOMS) >= 128 or identity != "owner" and sum(r["owner"] == identity for r in ROOMS.values()) >= 7:
                        result = (429, {"error": "Too many pending invitations"})
                    else:
                        room_id = secrets.token_hex(16)
                        room = {"host": secrets.token_urlsafe(32), "guest": secrets.token_urlsafe(32), "resume": secrets.token_urlsafe(32), "activated": False, "offer": offer, "answer": None, "generation": 0, "reconnect": False, "expires": now + 600, "owner": identity, "relay": credentials(14400 if identity != "owner" else 86400)}
                        ROOMS[room_id] = room
                        result = (201, {"id": room_id, "host": room["host"], "guest": room["guest"], "resume": room["resume"]})
                return self.reply(*result)
            match = re.fullmatch(r"/v1/rooms/([a-f0-9]{32})(/answer|/offer|/reconnect|/activate)?", self.path)
            if not match:
                return self.reply(404, {"error": "Not found"})
            body = self.body() if self.command == "POST" else {}
            answer = body.get("answer")
            offer = body.get("offer")
            if self.command == "POST" and match[2] == "/answer":
                if not isinstance(answer, str) or not answer.startswith("quaternio-peer-v1:") or len(answer) > 512 * 1024:
                    raise ValueError("Invalid reply")
            if self.command == "POST" and match[2] == "/offer":
                if not isinstance(offer, str) or not offer.startswith("quaternio-peer-v1:") or len(offer) > 512 * 1024:
                    raise ValueError("Invalid offer")
            with LOCK:
                room = ROOMS.get(match[1])
                if room is None:
                    response = (404, {"error": "Session ended or invitation expired"})
                else:
                    suffix = match[2]
                    host = self.token(room["host"])
                    guest = self.token(room["guest"]) and not room["activated"]
                    resumed = self.token(room["resume"])
                    generation = room["generation"]
                    response = (401, {"error": "Unauthorized"})
                    if self.command == "DELETE" and host and not suffix:
                        del ROOMS[match[1]]
                        response = (200, {"ok": True})
                    elif self.command == "GET" and suffix == "/answer" and host:
                        if room["answer"] is not None or generation > 0:
                            room["expires"] = now + 90
                        if int(room["relay"]["username"].split(":", 1)[0]) < time.time() + 600:
                            room["relay"] = credentials(14400 if room["owner"] != "owner" else 86400)
                        response = (200, {"answer": room["answer"], "generation": generation, "reconnect": room["reconnect"], "relay": room["relay"]})
                    elif self.command == "GET" and not suffix and (guest or resumed):
                        response = (200, {"offer": room["offer"], "relay": room["relay"], "generation": generation})
                    elif self.command == "POST" and suffix == "/answer" and (guest and generation == 0 or resumed):
                        if body.get("generation", 0) != generation:
                            response = (409, {"error": "Stale connection reply"})
                        elif room["answer"] is not None and room["answer"] != answer:
                            response = (409, {"error": "Invitation already used"})
                        elif sum(len(r["offer"]) + len(r["answer"] or "") for r in ROOMS.values()) + len(answer) - len(room["answer"] or "") > ROOM_BUDGET:
                            response = (429, {"error": "Coordination storage limit reached"})
                        else:
                            room["answer"] = answer
                            room["expires"] = now + 90
                            response = (200, {"ok": True})
                    elif self.command == "POST" and suffix == "/reconnect" and resumed:
                        if body.get("generation") == generation and room["answer"] is not None and not room["reconnect"]:
                            if allowed(("resume", match[1]), 4, 60):
                                room["reconnect"] = True
                                response = (200, {"ok": True, "generation": generation})
                            else:
                                response = (429, {"error": "Reconnect attempts are limited; retry shortly"})
                        else:
                            response = (200, {"ok": True, "generation": generation})
                    elif self.command == "POST" and suffix == "/activate" and host:
                        room["activated"] = True
                        response = (200, {"ok": True})
                    elif self.command == "POST" and suffix == "/offer" and host:
                        if not room["reconnect"]:
                            response = (409, {"error": "The guest has not requested reconnection"})
                        elif sum(len(r["offer"]) + len(r["answer"] or "") for r in ROOMS.values()) - len(room["offer"]) - len(room["answer"] or "") + len(offer) > ROOM_BUDGET:
                            response = (429, {"error": "Coordination storage limit reached"})
                        else:
                            room["offer"] = offer
                            room["generation"] += 1
                            room["answer"] = None
                            room["reconnect"] = False
                            room["expires"] = now + 90
                            room["relay"] = credentials(14400 if room["owner"] != "owner" else 86400)
                            response = (200, {"ok": True, "generation": room["generation"], "relay": room["relay"]})
            return self.reply(*response)
        except (ValueError, TypeError, json.JSONDecodeError):
            self.reply(400, {"error": "Invalid request"})
        except (TimeoutError, ConnectionError, OSError):
            self.close_connection = True

    do_GET = handle_request
    do_POST = handle_request
    do_DELETE = handle_request


class Server(ThreadingHTTPServer):
    daemon_threads = True
    slots = threading.BoundedSemaphore(96)

    def process_request(self, request, address):
        if not self.slots.acquire(blocking=False):
            self.close_request(request)
            return
        try:
            super().process_request(request, address)
        except BaseException:
            self.slots.release()
            raise

    def process_request_thread(self, request, address):
        try:
            super().process_request_thread(request, address)
        finally:
            self.slots.release()


if __name__ == "__main__":
    Server((os.environ.get("QUATERNIO_SIGNAL_BIND", "127.0.0.1"), int(os.environ.get("QUATERNIO_SIGNAL_PORT", "8787"))), Handler).serve_forever()
