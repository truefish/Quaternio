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
        if used + amount > 1024 * 1024 * 1024 or len(TRAFFIC) >= 512 and token not in TRAFFIC:
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
                match = re.fullmatch(r"/v1/tunnels/([a-f0-9]{32})", self.path)
                tunnel = TUNNELS.get(match[1]) if match else None
                result = (404, {"error": "Relay connection expired"})
        if tunnel is None:
            return self.reply(*result)
        if not secrets.compare_digest(tunnel.token, token):
            return self.reply(401, {"error": "Unauthorized"})
        tunnel.used = time.monotonic()
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
                    if len(ROOMS) >= 128 or identity != "owner" and sum(r["owner"] == identity for r in ROOMS.values()) >= 7:
                        result = (429, {"error": "Too many pending invitations"})
                    else:
                        room_id = secrets.token_hex(16)
                        room = {"host": secrets.token_urlsafe(32), "guest": secrets.token_urlsafe(32), "offer": offer, "answer": None, "expires": now + 600, "owner": identity, "relay": credentials(14400 if identity != "owner" else 86400)}
                        ROOMS[room_id] = room
                        result = (201, {"id": room_id, "host": room["host"], "guest": room["guest"]})
                return self.reply(*result)
            match = re.fullmatch(r"/v1/rooms/([a-f0-9]{32})(/answer)?", self.path)
            if not match:
                return self.reply(404, {"error": "Not found"})
            answer = None
            if self.command == "POST" and match[2]:
                with LOCK:
                    room = ROOMS.get(match[1])
                    authorized = room is not None and self.token(room["guest"])
                if not authorized:
                    return self.reply(401, {"error": "Unauthorized"})
                answer = self.body().get("answer")
                if not isinstance(answer, str) or not answer.startswith("quaternio-peer-v1:") or len(answer) > 512 * 1024:
                    raise ValueError("Invalid reply")
            with LOCK:
                room = ROOMS.get(match[1])
                if room is None:
                    response = (404, {"error": "Invitation expired"})
                else:
                    answer_path = bool(match[2])
                    host = self.token(room["host"])
                    guest = self.token(room["guest"])
                    response = (401, {"error": "Unauthorized"})
                    if self.command == "DELETE" and host and not answer_path:
                        del ROOMS[match[1]]
                        response = (200, {"ok": True})
                    elif self.command == "GET" and answer_path and host:
                        response = (200, {"answer": room["answer"]})
                    elif self.command == "GET" and not answer_path and guest:
                        response = (200, {"offer": room["offer"], "relay": room["relay"]})
                    elif self.command == "POST" and answer_path and guest:
                        if room["answer"] is not None and room["answer"] != answer:
                            response = (409, {"error": "Invitation already used"})
                        else:
                            room["answer"] = answer
                            response = (200, {"ok": True})
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
