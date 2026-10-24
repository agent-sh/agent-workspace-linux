"""Small bounded JSON-RPC client for the native acceptance tests."""
import json
import os
import select
import subprocess
import time


class Mcp:
    def __init__(self, binary, log, env=None, headless=False):
        self.log = open(log, "wb")
        self.process = subprocess.Popen([binary, "mcp", *(["--headless"] if headless else [])],
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log,
                                        env=env, bufsize=0)
        self.buffer = b""
        self.sequence = 0
        self.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                 "clientInfo": {"name": "kwin-acceptance", "version": "1"}})
        self.write({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def write(self, value):
        self.process.stdin.write((json.dumps(value) + "\n").encode())
        self.process.stdin.flush()

    def call(self, method, params, timeout=60, on_wait=None):
        self.sequence += 1
        id = self.sequence
        self.write({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                reply = json.loads(line)
                if reply.get("id") == id:
                    assert "error" not in reply, reply
                    return reply["result"]
                continue
            if on_wait:
                on_wait()
            if not select.select([self.process.stdout], [], [], min(0.1, max(0, deadline - time.monotonic())))[0]:
                continue
            chunk = os.read(self.process.stdout.fileno(), 65536)
            assert chunk, f"MCP exited: {self.process.poll()}"
            self.buffer += chunk
        raise TimeoutError(f"MCP {method} timed out")

    def tool(self, name, arguments, **options):
        result = self.call("tools/call", {"name": name, "arguments": arguments}, **options)
        assert not result.get("isError"), result
        if "structuredContent" in result:
            return result["structuredContent"]
        texts = [item["text"] for item in result.get("content", []) if item["type"] == "text"]
        assert texts, result
        return json.loads(texts[0])

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.log.close()
