# Scripted DeepSeek SSE mock. argv: port script.json. Script: list of
# {"tool": [name, args]} or {"text": "..."}; served in order, then text "end".
import json, sys, http.server, threading
port, script = int(sys.argv[1]), json.load(open(sys.argv[2]))
lock = threading.Lock(); n = [0]
class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def do_POST(self):
        self.rfile.read(int(self.headers["content-length"]))
        with lock:
            r = script[n[0]] if n[0] < len(script) else {"text": "end"}; n[0] += 1
        self.send_response(200); self.send_header("content-type", "text/event-stream")
        self.send_header("connection", "close"); self.end_headers()
        u = {"prompt_tokens": 1000, "completion_tokens": 100, "prompt_cache_hit_tokens": 600, "prompt_cache_miss_tokens": 400}
        if "tool" in r:
            name, args = r["tool"]
            d = {"reasoning_content": "r", "tool_calls": [{"index": 0, "id": "c%d" % n[0], "type": "function", "function": {"name": name, "arguments": json.dumps(args)}}]}
            fin = "tool_calls"
        else:
            d = {"reasoning_content": "r", "content": r["text"]}; fin = "stop"
        for ev in [{"choices": [{"index": 0, "delta": d, "finish_reason": None}]},
                   {"choices": [{"index": 0, "delta": {}, "finish_reason": fin}], "usage": u}]:
            self.wfile.write(("data: " + json.dumps(ev) + "\n\n").encode()); self.wfile.flush()
        self.wfile.write(b"data: [DONE]\n\n"); self.wfile.flush()
http.server.ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
