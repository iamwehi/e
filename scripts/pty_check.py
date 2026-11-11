import os, pty, sys, time, json, subprocess, signal, select, re
# Terminal checks for `e` in a real pty. Build first: cargo build --release
# Run: python3 scripts/pty_check.py
import tempfile
HERE = os.path.dirname(os.path.abspath(__file__))
E = os.path.join(HERE, "..", "target", "release", "e")
S = tempfile.mkdtemp(prefix="e-pty-")
def mock(port, script):
    p = os.path.join(S, "script%d.json" % port); json.dump(script, open(p, "w"))
    proc = subprocess.Popen([sys.executable, os.path.join(HERE, "mock_api.py"), str(port), p]); time.sleep(0.5); return proc
def setup(name, port):
    d = os.path.join(S, name); subprocess.run(["rm", "-rf", d]); os.makedirs(d)
    open(os.path.join(d, "cfg.toml"), "w").write('base_url = "http://127.0.0.1:%d"\n' % port); return d
class Pty:
    def __init__(self, d, inner):
        env = dict(os.environ, E_CONFIG=os.path.join(d, "cfg.toml"), VENICE_API_KEY="k", TERM="xterm")
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(d); os.execvpe("bash", ["bash", "--norc", "-c", "stty -g > before; " + inner + "; echo EXIT=$?; stty -g > after"], env)
        self.out = b""
    def read_until(self, pat, timeout=10):
        end = time.time() + timeout
        while time.time() < end:
            if re.search(pat, self.out): return True
            r, _, _ = select.select([self.fd], [], [], 0.1)
            if r:
                try: self.out += os.read(self.fd, 65536)
                except OSError: break
        return bool(re.search(pat, self.out))
    def send(self, b): os.write(self.fd, b)
    def child_e(self):
        return int(subprocess.run(["pgrep", "-P", str(self.pid), "-x", "e"], capture_output=True, text=True).stdout.split()[0])
def alive(pid):
    try: return not open("/proc/%d/stat" % pid).read().rsplit(")", 1)[1].strip().startswith("Z")
    except FileNotFoundError: return False
def stty_same(d): return open(os.path.join(d, "before")).read() == open(os.path.join(d, "after")).read()
def check(label, ok): print(("PASS " if ok else "FAIL ") + label); return ok

PROMPT = rb"deepseek-v4-1-flash \$[0-9.]+ > "
results = []
# A: escape sequences, Ctrl-C mid-command, Ctrl-D
m = mock(8801, [
    {"tool": ["bash", {"command": "printf '\\033[2J\\033]0;PWNED\\007\\033[31mRED\\033[0m \\033[?1049h\\n'"}]},
    {"text": "done \u001b[5mblink\u001b]52;c;Zm9v\u0007"},
    {"tool": ["bash", {"command": "echo $$ > sleeper.pid; sleep 60"}]},
    {"text": "after interrupt"}])
d = setup("a", 8801); p = Pty(d, E + " --verbose")
p.read_until(PROMPT); n = len(p.out); p.send(b"first\r")
results.append(check("A escape run returns to prompt", p.read_until(rb"(?s)done blink.*" + PROMPT, 10)))
body = p.out[n:]
results.append(check("A no foreign escape sequences printed", not re.search(rb"\x1b\[2J|\x1b\]|\x1b\[\?1049|\x1b\[5m|\x1b\[31mRED", body)))
results.append(check("A tool output visible (sanitized)", b"RED" in body and b"blink" in body))
n = len(p.out); p.send(b"second\r"); time.sleep(1.5)
t = time.time(); p.send(b"\x03")
ok = p.read_until(rb"interrupted\]", 5) and p.read_until(PROMPT, 5)
results.append(check("A Ctrl-C returns to prompt in %.2fs" % (time.time() - t), ok))
sp = int(open(os.path.join(d, "sleeper.pid")).read()); time.sleep(0.2)
results.append(check("A sleep process group killed", not alive(sp)))
n = len(p.out); p.send(b"third\r")
results.append(check("A next run works after interrupt", p.read_until(rb"after interrupt", 5)))
p.read_until(rb"(?s)after interrupt.*" + PROMPT, 5); p.send(b"\x04")
results.append(check("A Ctrl-D exits 0", p.read_until(rb"EXIT=0", 5)))
time.sleep(0.3); results.append(check("A terminal state restored", stty_same(d)))
m.terminate()

# B: SIGTERM during a run
m = mock(8802, [{"tool": ["bash", {"command": "echo $$ > sleeper.pid; sleep 60"}]}])
d = setup("b", 8802); p = Pty(d, E)
p.read_until(PROMPT); p.send(b"go\r"); time.sleep(1.5)
os.kill(p.child_e(), signal.SIGTERM)
results.append(check("B SIGTERM exits 143", p.read_until(rb"EXIT=143", 5)))
time.sleep(0.3); sp = int(open(os.path.join(d, "sleeper.pid")).read())
results.append(check("B child group killed on SIGTERM", not alive(sp)))
results.append(check("B terminal restored", stty_same(d)))
m.terminate()

# C: SIGTERM at the prompt (line editor in raw mode)
m = mock(8803, [])
d = setup("c", 8803); p = Pty(d, E)
p.read_until(PROMPT); p.send(b"half-typed"); time.sleep(0.5)
os.kill(p.child_e(), signal.SIGTERM)
results.append(check("C SIGTERM at prompt exits 143", p.read_until(rb"EXIT=143", 5)))
time.sleep(0.3); results.append(check("C raw mode undone", stty_same(d)))
# C2: SIGHUP and SIGINT at the prompt keep things sane
d = setup("c2", 8803); p = Pty(d, E)
p.read_until(PROMPT); os.kill(p.child_e(), signal.SIGINT); time.sleep(0.3); p.send(b"\x04")
results.append(check("C2 external SIGINT at prompt ignored, Ctrl-D exits", p.read_until(rb"EXIT=0", 5)))
time.sleep(0.3)
results.append(check("C2 terminal restored", stty_same(d)))
m.terminate()

# D: stdout piped to a file (interactive and headless)
m = mock(8804, [{"tool": ["bash", {"command": "printf '\\033[31mRED\\n'"}]}, {"text": "fin \u001b[1mbold"},
                {"tool": ["bash", {"command": "printf '\\033[31mRED\\n'"}]}, {"text": "headless \u001b[1mfinal"}])
d = setup("d", 8804); p = Pty(d, E + " --verbose > out.txt")
time.sleep(1); p.send(b"first\r"); time.sleep(2); p.send(b"\x04")
p.read_until(rb"EXIT=", 5)
out = open(os.path.join(d, "out.txt"), "rb").read()
results.append(check("D interactive piped stdout has no ESC (%d bytes)" % len(out), b"\x1b" not in out and b"fin bold" in out))
r = subprocess.run([E, "-p", "go", "--verbose"], cwd=d, capture_output=True, env=dict(os.environ, E_CONFIG=os.path.join(d, "cfg.toml"), VENICE_API_KEY="k"))
results.append(check("D headless: exit 0, final message on stdout, no ESC anywhere",
    r.returncode == 0 and r.stdout == b"headless final\n" and b"\x1b" not in r.stderr + r.stdout))
r = subprocess.run([E, "-p", "go", "--json"], cwd=d, capture_output=True, env=dict(os.environ, E_CONFIG=os.path.join(d, "cfg.toml"), VENICE_API_KEY="k"))
lines = [json.loads(l) for l in r.stdout.splitlines()]
results.append(check("D --json: valid JSONL events ending in run_end", r.returncode == 0 and lines[-1]["type"] == "run_end"))
m.terminate()
# E: the prompt opened in $EDITOR by editor_key, put back at the prompt, then sent
m = mock(8805, [{"text": "got it"}])
d = setup("e", 8805)
open(os.path.join(d, "ed.sh"), "w").write('printf "%s with more\\nand a second line\\n" "$(cat "$1")" > "$1"\n')
p = Pty(d, "EDITOR='sh ed.sh' " + E)
p.read_until(PROMPT); p.send(b"draft"); time.sleep(0.3); n = len(p.out); p.send(b"\x18\x05")
results.append(check("E Ctrl-X Ctrl-E puts the edited text back at the prompt", p.read_until(rb"(?s)" + PROMPT + rb"draft with more.*and a second line", 5)))
n = len(p.out); p.send(b"\r")
results.append(check("E Enter sends it", p.read_until(rb"(?s)got it.*" + PROMPT, 5)))
tr = open(next(os.path.join(r, f) for r, _, fs in os.walk(os.path.join(d, ".e", "runs")) for f in fs if f == "transcript.jsonl")).read()
results.append(check("E the model got the multi-line prompt", "draft with more\\nand a second line" in tr))
p.send(b"\x04"); p.read_until(rb"EXIT=0", 5); time.sleep(0.3)
results.append(check("E terminal restored", stty_same(d)))
# E2: an editor that fails leaves the prompt as it was; editor_key is configurable
open(os.path.join(d, "cfg.toml"), "a").write('editor_key = "ctrl-g"\n')
p = Pty(d, "VISUAL=false " + E)
p.read_until(PROMPT); p.send(b"keep me"); time.sleep(0.3); p.send(b"\x07")
results.append(check("E2 failing editor: notice, prompt unchanged", p.read_until(rb"(?s)editor: false exited.*prompt unchanged.*" + PROMPT + rb"keep me", 5)))
p.send(b"\x15\x04"); results.append(check("E2 Ctrl-D exits 0", p.read_until(rb"EXIT=0", 5)))
m.terminate()

# F: line editor at the prompt: Ctrl-C clears the line, /model updates the prompt, history recall
m = mock(8806, [{"text": "one"}, {"text": "two"}])
d = setup("f", 8806); p = Pty(d, E)
p.read_until(PROMPT); p.send(b"never sent\x03")
results.append(check("F Ctrl-C at prompt gives a fresh prompt", p.read_until(rb"(?s)never sent.*" + PROMPT, 5)))
p.send(b"/model zz\r")
results.append(check("F /model notice is coloured on a tty", p.read_until(rb"\x1b\[33m\[no prices for zz", 5)))
results.append(check("F prompt shows the new model", p.read_until(rb"zz \$0\.0000 > ", 5)))
p.send(b"hello\r")
results.append(check("F run after /model counts unpriced tokens", p.read_until(rb"(?s)one.*zz 1\.1k tok > ", 5)))
p.send(b"\x1b[A\r")
results.append(check("F up arrow recalls the last line", p.read_until(rb"(?s)1\.1k tok > .*hello.*two.*zz 2\.2k tok > ", 5)))
p.send(b"\x04")
results.append(check("F Ctrl-D exits 0", p.read_until(rb"EXIT=0", 5)))
time.sleep(0.3); results.append(check("F terminal restored", stty_same(d)))
m.terminate()
print("ALL PASS" if all(results) else "SOME FAILED")
sys.exit(0 if all(results) else 1)
