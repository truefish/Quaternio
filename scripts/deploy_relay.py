#!/usr/bin/env python3
"""Deploy Quaternio coordination and fallback TURN on a public Linux Docker host."""
import argparse
import errno
import ipaddress
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import socket
import subprocess
import time
import urllib.request


def ports_free(kind, host, ports):
    sockets = []
    try:
        for port in ports:
            connection = socket.socket(socket.AF_INET, kind)
            sockets.append(connection)
            connection.bind((host, port))
            if host == "0.0.0.0" and kind == socket.SOCK_STREAM and socket.has_ipv6:
                try:
                    ipv6 = socket.socket(socket.AF_INET6, kind)
                    sockets.append(ipv6)
                    ipv6.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
                    ipv6.bind(("::", port))
                except OSError as error:
                    if error.errno not in (errno.EAFNOSUPPORT, errno.EPROTONOSUPPORT, errno.EADDRNOTAVAIL):
                        raise
        return True
    except OSError:
        return False
    finally:
        for connection in sockets:
            connection.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--domain", required=True, help="DNS name pointing at this public server")
    parser.add_argument("--public-ip", help="Public IPv4 address; defaults to the domain's A record")
    parser.add_argument("--relay-ip", help="Local IPv4 interface for relay sockets; defaults to the outbound interface")
    parser.add_argument("--directory", type=Path, default=Path.home() / ".local/share/quaternio-relay")
    parser.add_argument("--prepare-only", action="store_true", help="Write configuration without starting services")
    parser.add_argument("--on-demand", action="store_true", help="Run until Enter or Ctrl+C, then stop the relay containers")
    access = parser.add_mutually_exclusive_group()
    access.add_argument("--public", dest="public", action="store_true", default=None, help="Enable rate-limited public session credentials; keep the operator key private")
    access.add_argument("--private", dest="public", action="store_false", help="Disable public session issuance")
    parser.add_argument("--proxy", choices=["auto", "caddy", "existing"], default="auto", help="Auto uses your existing proxy when 80/443 are occupied")
    parser.add_argument("--http-port", type=int, help="Internal HTTP port; automatically chooses an available port")
    parser.add_argument("--http-bind", default="127.0.0.1", help="Private interface reachable by your proxy; defaults to localhost")
    parser.add_argument("--turn-port", type=int, help="TURN UDP listener; automatically chooses an available port")
    parser.add_argument("--relay-range", help="UDP relay range, e.g. 50000:50040; automatically chooses a free range")
    args = parser.parse_args()
    domain = args.domain.encode("idna").decode("ascii").lower()
    if len(domain) > 253 or "." not in domain or not all(re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", part) for part in domain.split(".")):
        parser.error("Use a DNS hostname such as relay.example.com")
    public_ip = args.public_ip or socket.gethostbyname(domain)
    address = ipaddress.ip_address(public_ip)
    if address.version != 4 or not address.is_global:
        parser.error("The relay needs a publicly reachable IPv4 server, not an address behind CGNAT")
    if os.name != "posix" and not args.prepare_only:
        parser.error("Run deployment on a public Linux Docker host")
    if args.relay_ip:
        relay_ip = args.relay_ip
    else:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as route:
            # Select the outbound interface without sending a packet.
            route.connect((public_ip, 3478))
            relay_ip = route.getsockname()[0]
    local_address = ipaddress.ip_address(relay_ip)
    if local_address.version != 4 or local_address.is_loopback or local_address.is_unspecified:
        parser.error("Use the server's actual IPv4 interface for --relay-ip")
    directory = args.directory.expanduser().resolve()
    if directory == Path(directory.anchor) or directory == Path.home().resolve():
        parser.error("Choose a dedicated relay directory")
    previous_path = directory / "compose.json"
    previous = json.loads(previous_path.read_text()) if previous_path.exists() else {}
    previous_env = previous.get("services", {}).get("signaling", {}).get("environment", {})
    public = args.public if args.public is not None else previous_env.get("QUATERNIO_PUBLIC") == "1"
    previous_settings = {}
    settings_path = directory / "deployment.json"
    if settings_path.exists():
        previous_settings = json.loads(settings_path.read_text())
    proxy = args.proxy
    if proxy == "auto":
        if previous:
            proxy = "caddy" if "https" in previous.get("services", {}) else "existing"
        else:
            proxy = "caddy" if ports_free(socket.SOCK_STREAM, "0.0.0.0", [80, 443]) else "existing"
    if proxy == "caddy" and "https" not in previous.get("services", {}) and not ports_free(socket.SOCK_STREAM, "0.0.0.0", [80, 443]):
        parser.error("Ports 80/443 are occupied. Use --proxy existing; existing applications will not be stopped")
    bind = ipaddress.ip_address(args.http_bind)
    if bind.version != 4 or not bind.is_private or bind.is_unspecified:
        parser.error("--http-bind must be a specific private or loopback IPv4 address reachable by your proxy")
    def choose_port(explicit, old, start, kind, host):
        if explicit is not None and not 1024 <= explicit <= 65535:
            parser.error("Custom ports must be between 1024 and 65535")
        if old is not None and (explicit is None or explicit == old):
            return old
        options = [explicit] if explicit is not None else range(start, start + 100)
        for port in options:
            if ports_free(kind, host, [port]):
                return port
        parser.error("Requested ports are occupied; choose different --http-port or --turn-port values")
    http_port = choose_port(args.http_port, int(previous_env["QUATERNIO_SIGNAL_PORT"]) if "QUATERNIO_SIGNAL_PORT" in previous_env else (8787 if previous else None), 8787, socket.SOCK_STREAM, args.http_bind)
    turn_port = choose_port(args.turn_port, int(previous_env["QUATERNIO_TURN_LOCAL_PORT"]) if "QUATERNIO_TURN_LOCAL_PORT" in previous_env else (3478 if previous else None), 3478, socket.SOCK_DGRAM, "0.0.0.0")
    old_range = previous_settings.get("relay_range", "49160:49200" if previous else None)
    ranges = [args.relay_range or old_range] if args.relay_range or old_range else [f"{port}:{port+40}" for port in range(49160, 60000, 41)]
    for value in ranges:
        try:
            low, high = map(int, value.split(":"))
        except (ValueError, AttributeError):
            parser.error("Use --relay-range START:END")
        if not 1024 <= low <= high <= 65535 or high-low > 1023 or low <= turn_port <= high:
            parser.error("Use a relay range of 1–1024 ports, excluding the TURN listener")
        if value == old_range or ports_free(socket.SOCK_DGRAM, "0.0.0.0", range(low, high+1)):
            break
    else:
        parser.error("UDP relay ports are occupied. Choose a different --relay-range")
    directory.mkdir(parents=True, exist_ok=True)
    directory.chmod(0o700)
    settings_path.write_text(json.dumps({"relay_range":f"{low}:{high}"}))
    credential_path = directory / "credentials.json"
    if credential_path.exists():
        credentials = json.loads(credential_path.read_text())
    else:
        credentials = {"key": secrets.token_urlsafe(32), "turn": secrets.token_hex(32)}
        with credential_path.open("x") as file:
            os.chmod(credential_path, 0o600)
            json.dump(credentials, file)
    if min(len(credentials.get("key", "")), len(credentials.get("turn", ""))) < 32:
        raise ValueError("Invalid existing relay credentials")
    source = Path(__file__).resolve().parents[1] / "relay/service.py"
    shutil.copy2(source, directory / "service.py")
    (directory / ".dockerignore").write_text("*\n!service.py\n!Dockerfile\n")
    (directory / "Dockerfile").write_text("FROM python:3.14-alpine\nWORKDIR /app\nCOPY service.py /app/service.py\nUSER 65532:65532\nCMD [\"python\", \"-u\", \"/app/service.py\"]\n")
    (directory / "Caddyfile").write_text(f"{{\n admin off\n}}\n{domain} {{\n request_body {{\n  max_size 768KB\n }}\n reverse_proxy {args.http_bind}:{http_port}\n}}\n")
    (directory / "nginx-location.conf").write_text(f"location / {{\n    proxy_pass http://{args.http_bind}:{http_port};\n    proxy_http_version 1.1;\n    proxy_set_header Host $host;\n    proxy_set_header Authorization $http_authorization;\n    proxy_set_header X-Forwarded-For $remote_addr;\n    proxy_buffering off;\n    proxy_cache off;\n    client_max_body_size 768k;\n    proxy_read_timeout 30s;\n}}\n")
    turn = f"""listening-port={turn_port}
listening-ip=0.0.0.0
relay-ip={relay_ip}
external-ip={public_ip}/{relay_ip}
min-port={low}
max-port={high}
realm={domain}
fingerprint
use-auth-secret
static-auth-secret={credentials['turn']}
no-cli
no-tls
no-dtls
no-tcp
no-multicast-peers
total-quota=128
user-quota=16
stale-nonce=600
log-file=stdout
simple-log
pidfile=/tmp/turnserver.pid
denied-peer-ip=0.0.0.0-0.255.255.255
denied-peer-ip=10.0.0.0-10.255.255.255
denied-peer-ip=127.0.0.0-127.255.255.255
denied-peer-ip=169.254.0.0-169.254.255.255
denied-peer-ip=172.16.0.0-172.31.255.255
denied-peer-ip=192.168.0.0-192.168.255.255
"""
    turn_path = directory / "turnserver.conf"
    turn_path.touch(mode=0o600, exist_ok=True)
    turn_path.chmod(0o600)
    turn_path.write_text(turn)
    if public:
        with turn_path.open("a") as file:
            # Public relay pairs stay on this service, rather than forwarding
            # arbitrary UDP packets to third-party Internet destinations.
            file.write(f"max-bps=262144\nbps-capacity=4194304\ndenied-peer-ip=::-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff\ndenied-peer-ip=0.0.0.0-255.255.255.255\nallowed-peer-ip={public_ip}\n")
    common = {"network_mode": "host", "restart": "no" if args.on_demand else "unless-stopped", "security_opt": ["no-new-privileges:true"]}
    compose = {"services": {
        "signaling": {**common, "build": {"context": "."}, "read_only": True, "cap_drop": ["ALL"], "pids_limit": 192, "mem_limit": "256m", "environment": {"QUATERNIO_RELAY_KEY": credentials["key"], "QUATERNIO_TURN_SECRET": credentials["turn"], "QUATERNIO_TURN_URLS": f"turn:{public_ip}:{turn_port}?transport=udp", "QUATERNIO_SIGNAL_BIND":args.http_bind, "QUATERNIO_SIGNAL_PORT":str(http_port), "QUATERNIO_TURN_LOCAL_PORT":str(turn_port)}},
        "turn": {**common, "image": "coturn/coturn:4.18.0-r0", "user": "0:0", "read_only": True, "cap_drop": ["ALL"], "cap_add": ["NET_BIND_SERVICE"], "tmpfs": ["/tmp:size=16m"], "pids_limit": 128, "mem_limit": "512m", "volumes": [f"{turn_path}:/etc/coturn/turnserver.conf:ro"], "command": ["-c", "/etc/coturn/turnserver.conf"]},
        "https": {**common, "image": "caddy:2-alpine", "volumes": [f"{directory / 'Caddyfile'}:/etc/caddy/Caddyfile:ro", "caddy-data:/data", "caddy-config:/config"]}
    }, "volumes": {"caddy-data": {}, "caddy-config": {}}}
    if proxy == "existing":
        del compose["services"]["https"]
        compose.pop("volumes")
    compose["services"]["signaling"]["environment"]["QUATERNIO_PUBLIC"] = "1" if public else "0"
    compose_path = directory / "compose.json"
    compose_path.touch(mode=0o600, exist_ok=True)
    compose_path.chmod(0o600)
    compose_path.write_text(json.dumps(compose, indent=2))
    print(f"Service address: https://{domain}")
    print("Access: public sessions enabled; operator key stays private." if public else "Access: private operator key required.")
    print(f"Proxy mode: {proxy}. Upstream: http://{args.http_bind}:{http_port}")
    print(f"Required inbound ports: {'TCP 80, 443; ' if proxy == 'caddy' else 'HTTPS at your existing proxy; '}UDP {turn_port} and {low}-{high}.")
    if proxy == "existing":
        print(f"Route https://{domain} to the upstream above. Preserve paths and Authorization; disable caching; allow 768 KiB bodies and 30s timeouts.")
        print(f"Nginx: include {directory / 'nginx-location.conf'} inside your existing HTTPS server block. Caddy example: {directory / 'Caddyfile'}")
    print("No existing application or proxy configuration is modified. Behind a CDN, pass --public-ip with the origin server's IPv4. Firewalls are not modified.")
    if args.prepare_only:
        print(f"Configuration prepared in {directory}; no services started.")
        return
    command = ["docker", "compose", "-p", "quaternio-relay", "-f", str(compose_path)]
    if args.on_demand:
        import atexit
        atexit.register(lambda: subprocess.run(command + ["down"], check=False))
    subprocess.run(command + ["up", "-d", "--build", "--pull", "always", "--remove-orphans"], check=True)
    check_url = f"https://{domain}" if proxy == "caddy" else f"http://{args.http_bind}:{http_port}"
    # Never send the private host key to a possibly occupied or misrouted port.
    request = urllib.request.Request(f"{check_url}/health")
    for _ in range(40):
        try:
            with urllib.request.urlopen(request, timeout=3) as response:
                result = json.load(response)
            if result.get("service") == "quaternio-relay" and (not public or result.get("public") is True):
                print("Relay HTTPS is ready." if proxy == "caddy" else "Backend is ready. Configure your proxy before inviting guests; public HTTPS has not been verified.")
                if public:
                    print("Public sessions need no operator key. Keep credentials.json private.")
                else:
                    print("In Quaternio → Collaborate → Connection options, enter the service address and this private host key:")
                    print(credentials["key"])
                    print("Keep the key private. Guests only need their invite code.")
                if args.on_demand:
                    print("On-demand mode: leave this terminal open while inviting or relaying. Direct connections continue if this service stops; relayed connections do not.")
                    try:
                        input("Press Enter or Ctrl+C to stop the private service. ")
                    except (KeyboardInterrupt, EOFError):
                        pass
                return
        except (OSError, ValueError):
            time.sleep(3)
    raise SystemExit(f"HTTPS is not ready. Check DNS, inbound ports and: docker compose -p quaternio-relay -f {compose_path} logs")


if __name__ == "__main__":
    main()
