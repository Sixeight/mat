"""Exercise CLI terminal I/O; this does not verify pixels in a terminal emulator."""

import argparse
import errno
import json
import os
import pathlib
import pty
import select
import struct
import subprocess
import tempfile
import threading
import time
import zlib
from http.server import BaseHTTPRequestHandler, HTTPServer


def png_chunk(kind, data):
    return (
        struct.pack(">I", len(data))
        + kind
        + data
        + struct.pack(">I", zlib.crc32(kind + data))
    )


def fixture_png():
    return (
        b"\x89PNG\r\n\x1a\n"
        + png_chunk(b"IHDR", struct.pack(">IIBBBBB", 16, 16, 8, 2, 0, 0, 0))
        + png_chunk(b"IDAT", zlib.compress((b"\0" + b"\xff\0\0" * 16) * 16))
        + png_chunk(b"IEND", b"")
    )


def fixture_svg():
    return (
        b'<?xml version="1.0"?>'
        b'<svg xmlns="http://www.w3.org/2000/svg" width="80" height="20">'
        b'<rect width="80" height="20" fill="green"/>'
        b'<text x="4" y="14" font-family="sans-serif" font-size="11" fill="white">CI pass</text>'
        b'</svg>'
    )


def run(binary, cwd, args, data=b"", extra=None, merge_stderr=False):
    env = os.environ.copy()
    for key in (
        "TERM", "TERM_PROGRAM", "TMUX", "KITTY_WINDOW_ID",
        "GHOSTTY_RESOURCES_DIR", "ITERM_SESSION_ID", "NO_COLOR",
    ):
        env.pop(key, None)
    env.update(TERM="xterm-kitty", TERM_PROGRAM="kitty")
    env.update(extra or {})
    master, slave = pty.openpty()
    proc = subprocess.Popen(
        [binary, *args], stdin=subprocess.PIPE, stdout=slave,
        stderr=slave if merge_stderr else subprocess.PIPE, cwd=cwd, env=env,
    )
    os.close(slave)
    try:
        proc.stdin.write(data)
        proc.stdin.close()
        output = bytearray()
        deadline = time.monotonic() + 15
        while True:
            if time.monotonic() > deadline:
                raise AssertionError("CLI did not exit without an input query")
            ready, _, _ = select.select([master], [], [], 0.1)
            if ready:
                try:
                    block = os.read(master, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not block:
                    break
                output.extend(block)
            elif proc.poll() is not None:
                break
        code = proc.wait(timeout=2)
        error = b"" if merge_stderr else proc.stderr.read()
        return code, bytes(output), error
    finally:
        os.close(master)
        if proc.poll() is None:
            proc.kill()
            proc.wait()


def check_http_images(binary, root):
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            requests.append(self.path)
            path = self.path.split("?", 1)[0]
            if path == "/image.png":
                data = fixture_png()
            elif path in ("/badge.svg", "/badge"):
                data = fixture_svg()
            elif path == "/invalid.svg":
                data = b'<svg xmlns="http://www.w3.org/2000/svg"><'
            else:
                data = b"invalid image"
            self.send_response(200)
            length = 16 * 1024 * 1024 + 1 if self.path == "/oversized.png" else len(data)
            self.send_header("Content-Length", str(length))
            if path.endswith(".svg"):
                self.send_header("Content-Type", "image/svg+xml")
            else:
                self.send_header("Content-Type", "application/octet-stream")
            self.end_headers()
            if self.path != "/oversized.png":
                self.wfile.write(data)

        def log_message(self, *_args):
            pass

    with HTTPServer(("127.0.0.1", 0), Handler) as server:
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        base_url = f"http://127.0.0.1:{server.server_port}"
        try:
            code, out, err = run(
                binary, root, ["--color", "never"],
                f"Before\n\n![Remote]({base_url}/image.png)\n\nAfter\n".encode(),
            )
            assert code == 0 and not err and b"\x1b_G" in out and b"After" in out
            assert out.index(b"\x1b_G") < out.index(b"After")
            code, out, err = run(
                binary,
                root,
                ["--image-loading", "async", "--color", "never"],
                f"Before\n\n![Remote]({base_url}/image.png)\n\nAfter\n".encode(),
            )
            assert code == 0 and not err and b"\x1b_G" in out and b"After" in out
            assert out.index(b"After") < out.index(b"\x1b_G")
            for path in ("badge.svg", "badge.svg?token=fixture", "badge"):
                code, out, err = run(
                    binary, root, ["--color", "never", "--verbose"],
                    f"Before\n\n![Badge]({base_url}/{path})\n\nAfter\n".encode(),
                )
                assert code == 0 and not err and b"\x1b_G" in out and b"After" in out
                assert b"[image:" not in out
            for path in ("invalid.png", "oversized.png", "invalid.svg"):
                code, out, err = run(
                    binary, root, ["--color", "never"],
                    f"Before\n\n![Remote]({base_url}/{path})\n\nAfter\n".encode(),
                )
                assert code == 0 and b"[image: Remote]" in out and b"After" in out
                assert not err and b"mat: image" not in out and b"\x1b_G" not in out
            code, out, err = run(
                binary, root, ["--color", "never", "--verbose"],
                (
                    f"Before\n\n![SVG]({base_url}/badge.svg)\n\n"
                    f"![Badge]({base_url}/badge)\n\n"
                    f"![Invalid]({base_url}/invalid.png)\n\n"
                    f"![Malformed SVG]({base_url}/invalid.svg)\n\n"
                    f"![Oversized]({base_url}/oversized.png)\n\nAfter\n"
                ).encode(),
                merge_stderr=True,
            )
            assert code == 0 and not err and b"\x1b_G" in out
            assert out.count(b"mat: image") == 3
            assert out.index(b"After") < out.index(b"mat: image")
            assert b"Failed to decode image" in out and b"MiB limit" in out
            count = len(requests)
            output = subprocess.run(
                [binary], input=f"![Remote]({base_url}/badge.svg)\n".encode(),
                capture_output=True, cwd=root, timeout=5,
            )
            assert output.returncode == 0 and not output.stderr
            assert len(requests) == count
        finally:
            server.shutdown()
            worker.join(timeout=2)
    return [
        "HTTP image renders through a local server",
        "async image loading prints the document before the image",
        "invalid HTTP image falls back quietly and preserves following text",
        "oversized HTTP image falls back quietly before reading the payload",
        "SVG badges render with extensions, query strings, and extensionless URLs",
        "malformed SVG falls back quietly and preserves following text",
        "verbose HTTP image diagnostics follow all document output on the same terminal",
        "redirected output does not fetch HTTP images",
    ]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=pathlib.Path)
    binary = str(parser.parse_args().binary.resolve())
    with tempfile.TemporaryDirectory(prefix="mat-pty-") as temp:
        root = pathlib.Path(temp)
        docs = root / "docs"
        docs.mkdir()
        (docs / "sample.png").write_bytes(fixture_png())
        markdown = "Before\n\n![Local](sample.png)\n\nAfter\n"
        (docs / "readme.md").write_text(markdown)
        checks = []

        code, out, err = run(binary, root, ["docs/readme.md", "--color", "never"])
        assert code == 0 and not err and b"\x1b_G" in out
        assert b"Before" in out and b"After" in out
        assert out.index(b"\x1b_G") < out.index(b"After")
        checks.append("file-relative image renders at an automatic nonzero width")

        code, out, err = run(
            binary, root, ["--base-dir", "docs", "--color", "never"], markdown.encode(),
        )
        assert code == 0 and not err and b"\x1b_G" in out and b"After" in out
        assert out.index(b"\x1b_G") < out.index(b"After")
        checks.append("piped stdin and base-dir render without an input query")

        (docs / "badge.svg").write_bytes(fixture_svg())
        code, out, err = run(
            binary, root, ["--base-dir", "docs", "--verbose", "--color", "never"],
            b"Before\n\n![Badge](badge.svg)\n\nAfter\n",
        )
        assert code == 0 and not err and b"\x1b_G" in out and b"After" in out
        assert b"[image:" not in out
        checks.append("local SVG badge renders through the terminal image protocol")

        code, out, err = run(
            binary, root, ["--color", "never"],
            b"Before\n\n![Missing](missing.png)\n\nAfter\n",
        )
        assert code == 0 and b"After" in out and b"[image: Missing]" in out
        assert not err and b"mat: image" not in out and b"\x1b_G" not in out
        checks.append("missing image falls back quietly and preserves following text")

        code, out, err = run(
            binary, root, ["--color", "never", "-v"],
            b"Before\n\n![Missing](missing.png)\n\nAfter\n",
            merge_stderr=True,
        )
        assert code == 0 and not err and b"[image: Missing]" in out
        assert b"cannot read image" in out and b"\x1b_G" not in out
        assert out.index(b"After") < out.index(b"mat: image")
        checks.append("verbose missing-image diagnostic follows document output on the same terminal")

        code, out, err = run(
            binary, root, ["docs/readme.md", "--images", "never", "--color", "never"],
        )
        assert code == 0 and not err and b"\x1b" not in out and b"[image: Local]" in out
        checks.append("images never emits a text fallback without graphics controls")

        code, out, err = run(
            binary, root, ["docs/readme.md", "--color", "never"],
            extra={"TERM": "tmux-256color", "TERM_PROGRAM": "tmux", "ITERM_SESSION_ID": "test"},
        )
        assert code == 0 and not err and "▀".encode() in out
        assert b"]1337;" not in out and b"After" in out
        checks.append("tmux with iTerm uses halfblocks and preserves following text")

        checks.extend(check_http_images(binary, root))

        print(json.dumps({"passed": len(checks), "checks": checks}))


if __name__ == "__main__":
    main()
