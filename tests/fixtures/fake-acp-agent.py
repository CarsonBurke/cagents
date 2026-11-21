#!/usr/bin/python3
"""Offline Agent Client Protocol (v1) agent for cagents' ACP chat adapter.

It answers over stdio like `agy acp` or `muse acp`, without network access,
credentials or a model, and never touches the real agents' data. Its replies
copy the shapes of Antigravity 1.5.12's and Muse 1.4.23's `initialize` and
`session/new` answers: by default Antigravity's, Muse's when it is run as
`muse` or `muse-upstream` or with FAKE_ACP_SHAPE=muse. Any command-line
arguments, such as `acp --disable-telemetry`, are accepted and ignored,
but for Antigravity's `--app-data-dir DIR`, which keeps that process's
conversations apart from the ones it lists otherwise, as `agy` keeps them
in ~/.gemini/DIR rather than ~/.gemini/antigravity-cli.

Each conversation is saved under FAKE_ACP_STATE (default: a directory in
the system's temporary directory) as the session updates it sent, so
`session/load` replays it and `session/list` lists it, newest first.

The prompt chooses the turn, by the first of these words it contains as a
word:

  tools       a plan, a shell command, a file edit with its diff, then an answer
  edits       one edit across several files (a two-hunk change, a new file and
              a deleted one), then a long rewrite of a single file
  permission  an edit that asks for approval first; refusing fails the edit,
              stopping the turn cancels it
  hold        thinks until the turn is stopped (at most 60 seconds)
  garbage     prints a line that is not JSON in the middle of its answer
  files       asks the client to read a file and reports what it was told
  mode        switches its own mode and reports its new config options
  title       names the conversation
  refuse      ends the turn with the `refusal` stop reason
  auth        refuses the prompt until the user signs in
  /compact    answers as a compaction command

Anything else is answered with a short thought and a streamed reply.

Environment switches:
  FAKE_ACP_SHAPE=agy|muse   which agent's replies to copy
  FAKE_ACP_STATE=DIR        where conversations are saved
  FAKE_ACP_AUTH=initialize|new|load|list|prompt
                            refuse that request as signed out (-32000)
  FAKE_ACP_NO_LOAD=1        do not offer session/load
  FAKE_ACP_NOISE=1          print a line that is not JSON before answering
                            `initialize`
  FAKE_ACP_REPLAY=N         a loaded conversation starts with N extra
                            exchanges, for long histories
  FAKE_ACP_LOG=FILE         append every frame received, one per line
  FAKE_ACP_LOGIN_SECONDS=N  how long `login` waits before signing in
  FAKE_ACP_LIST_PAGE=N      list N conversations a page, each page after
                            the first by the `nextCursor` of the one before
  FAKE_ACP_PRESENCE=1       hold the lock the agent holds on a conversation
                            while it has it open, under $HOME as the real
                            agent does: Antigravity's
                            ~/.gemini/antigravity-cli/presence/ID.lock, which
                            it leaves behind, and Muse's
                            $XDG_DATA_HOME/muse/sessions/YYYY/MM/DD/ID/.session.lock.
                            Only for a $HOME that is the fixture's own.

In the Muse shape, `usage [--json]`, `login` and `logout` answer like Muse's
account commands. Signing out lasts, as a file under FAKE_ACP_STATE, until
`login`.
"""
import fcntl
import json
import os
import queue
import re
import sys
import tempfile
import threading
import time
import uuid

SHAPE = os.environ.get("FAKE_ACP_SHAPE") or (
    "muse" if os.path.basename(sys.argv[0]).startswith("muse") else "agy"
)
# Never a real data directory: without FAKE_ACP_STATE, conversations last
# only as long as the temporary directory does.
STATE = os.environ.get("FAKE_ACP_STATE") or os.path.join(
    tempfile.gettempdir(), f"fake-acp-agent-{os.getuid()}"
)
# Antigravity's own data directory, relative to ~/.gemini, for the
# conversations a process started with `--app-data-dir` keeps apart.
APP_DATA = "antigravity-cli"
for _index, _argument in enumerate(sys.argv[1:]):
    if SHAPE != "agy":
        break
    if _argument == "--app-data-dir" and _index + 2 < len(sys.argv):
        APP_DATA = sys.argv[_index + 2]
    elif _argument.startswith("--app-data-dir="):
        APP_DATA = _argument.split("=", 1)[1]
if APP_DATA != "antigravity-cli":
    STATE = os.path.join(STATE, "app-data", APP_DATA.replace("/", "_"))
AUTH = os.environ.get("FAKE_ACP_AUTH", "")
LOG = os.environ.get("FAKE_ACP_LOG")

AGY_MODES = [
    ("default", "Default", "Ask before edits and commands, per your permission settings."),
    ("accept-edits", "Accept edits", "Approve file edits in the workspace without asking."),
    ("auto", "Auto", "Approve edits and safe commands; ask before irreversible actions."),
    ("plan", "Plan", "Plan carefully before executing a task."),
]
AGY_MODELS = [
    ("gemini-3.8-flash", "Gemini 3.8 Flash"),
    ("gemini-4-neon", "Gemini 4 Neon"),
    ("gemini-pro-orchestrator-neon", "Orchestrator (Neon)"),
    ("gpt-oss-120b-medium", "GPT-OSS 120B (Medium)"),
]
AGY_LEVELS = [("low", "Low"), ("medium", "Medium"), ("high", "High"), ("xhigh", "Extra")]
# As agy 1.x lists them: its thinking option names every level, but each
# model offers its own, which its `models` state spells as combined
# identifiers. Flash has no Extra, and some models take no level at all.
AGY_MODEL_LEVELS = {
    "gemini-3.8-flash": ["low", "medium", "high"],
    "gemini-4-neon": ["low", "medium", "high", "xhigh"],
}
MUSE_MODES = [
    ("allowAll", "Allow all", "Run every action without asking for approval"),
    ("promptUnmatched", "Ask before unmatched actions",
     "Ask for approval when no permission rule covers the action"),
    ("onRequest", "Ask on request", "Ask for approval only when the action requires it"),
    ("denyUnmatched", "Deny unmatched actions", "Refuse any action no permission rule covers"),
]
MUSE_MODELS = [("muse-spark-1.2", "muse-spark-1.2"), ("muse-spark-1.2-contributor",
                                                    "muse-spark-1.2-contributor"),
               ("muse-wonder-1.0", "muse-wonder-1.0")]
# Muse 1.4.24 offers a reasoning option only for some models, listing just
# the tiers its catalog describes, with the description as the name.
MUSE_LEVELS = {
    "muse-spark-1.2": [("xhigh", "Use this for deepest analysis and complex fixes.")],
    "muse-spark-1.2-contributor": [("xhigh", "Use this for deepest analysis and complex fixes.")],
}
# Muse names a new session with two random words before it is asked anything.
MUSE_NAMES = ["cyan-wezen", "quiet-heron", "weathered-planetesimal"]
COMMANDS = [
    {"name": "compact", "description": "Summarize the conversation to free context"},
    {"name": "review", "description": "Review the current changes",
     "input": {"hint": "what to review"}},
]

out_lock = threading.Lock()


def send(value):
    with out_lock:
        sys.stdout.write(json.dumps(value) + "\n")
        sys.stdout.flush()


def noise(text):
    with out_lock:
        sys.stdout.write(text + "\n")
        sys.stdout.flush()


def reply(id, result):
    send({"jsonrpc": "2.0", "id": id, "result": result})


def fail(id, code, message):
    send({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})


def signed_out(id):
    if SHAPE == "muse":
        fail(id, -32000, "log in to Muse before opening a session")
    else:
        fail(id, -32000, "Authentication required: sign in to the fake agent")


def logged_out():
    """Signed out until `login`: the state's marker, written by `logout`."""
    return os.path.exists(os.path.join(STATE, "signed-out"))


class Session:
    def __init__(self, id, cwd):
        self.id = id
        self.cwd = cwd
        if SHAPE == "muse":
            self.mode, self.model, self.level = "onRequest", "muse-spark-1.2", None
        else:
            self.mode, self.model, self.level = "default", "gemini-3.8-flash", "low"
        self.title = None
        self.lock = None

    def hold_presence(self):
        """Locks the conversation as the agent does while it has it open."""
        if not os.environ.get("FAKE_ACP_PRESENCE") or self.lock is not None:
            return
        home = os.environ["HOME"]
        if SHAPE == "muse":
            data = os.environ.get("XDG_DATA_HOME") or os.path.join(home, ".local", "share")
            path = os.path.join(data, "muse", "sessions", time.strftime("%Y/%m/%d"),
                                self.id, ".session.lock")
        else:
            path = os.path.join(home, ".gemini", APP_DATA, "presence", self.id + ".lock")
        os.makedirs(os.path.dirname(path), exist_ok=True)
        self.lock = open(path, "a")
        fcntl.flock(self.lock, fcntl.LOCK_EX)

    def release_presence(self):
        if self.lock is not None:
            self.lock.close()
            self.lock = None

    def path(self):
        return os.path.join(STATE, "sessions", self.id + ".jsonl")

    def update(self, update, record=True):
        send({"jsonrpc": "2.0", "method": "session/update",
              "params": {"sessionId": self.id, "update": update}})
        if record and update["sessionUpdate"] in (
                "user_message_chunk", "agent_message_chunk", "agent_thought_chunk",
                "tool_call", "tool_call_update", "plan"):
            os.makedirs(os.path.dirname(self.path()), exist_ok=True)
            with open(self.path(), "a") as saved:
                saved.write(json.dumps(update) + "\n")

    def modes(self):
        return AGY_MODES if SHAPE == "agy" else MUSE_MODES

    def config_options(self):
        mode = {"id": "mode", "name": "Mode", "category": "mode", "type": "select",
                "currentValue": self.mode,
                "options": [{"value": v, "name": n, "description": d} for v, n, d in self.modes()]}
        models = AGY_MODELS if SHAPE == "agy" else MUSE_MODELS
        model = {"id": "model", "name": "Model", "category": "model", "type": "select",
                 "currentValue": self.model,
                 "options": [{"value": v, "name": n} for v, n in models]}
        if SHAPE == "muse":
            levels = MUSE_LEVELS.get(self.model)
            if not levels:
                return [mode, model]
            if self.level not in [v for v, _ in levels]:
                self.level = levels[0][0]
            return [mode, model,
                    {"id": "thought_level", "name": "Reasoning effort",
                     "category": "thought_level", "type": "select", "currentValue": self.level,
                     "options": [{"value": v, "name": n} for v, n in levels]}]
        level = {"id": "thought_level", "name": "Thinking Level", "category": "thought_level",
                 "type": "select", "currentValue": self.level,
                 "options": [{"value": v, "name": n} for v, n in AGY_LEVELS]}
        return [model, level, mode]

    def agy_combined_models(self):
        names = dict(AGY_LEVELS)
        combined = []
        for v, n in AGY_MODELS:
            levels = AGY_MODEL_LEVELS.get(v)
            if not levels:
                combined.append({"modelId": v, "name": n})
            combined.extend({"modelId": f"{v}-{level}", "name": f"{n} ({names[level]})"}
                            for level in levels or [])
        return combined

    def setup(self, session_id=True):
        result = {
            "modes": {"currentModeId": self.mode,
                      "availableModes": [{"id": v, "name": n, "description": d}
                                         for v, n, d in self.modes()]},
            "configOptions": self.config_options(),
        }
        if SHAPE == "agy":
            result["models"] = {
                "availableModels": self.agy_combined_models(),
                "currentModelId": (f"{self.model}-{self.level}"
                                   if self.level in AGY_MODEL_LEVELS.get(self.model, [])
                                   else self.model),
            }
            result["_meta"] = {"antigravity": {"tier": {"name": "Fixture tier", "id": "fixture"}}}
        if session_id:
            result["sessionId"] = self.id
        return result


class Agent:
    def __init__(self):
        self.inbox = queue.Queue()
        self.sessions = {}
        self.next_request = 0
        self.responses = {}
        self.cancelled = set()

    def read(self):
        # Whatever ends the input, the agent hears that it ended and exits.
        try:
            for line in sys.stdin:
                line = line.strip()
                if not line:
                    continue
                if LOG:
                    with open(LOG, "a") as log:
                        log.write(line + "\n")
                try:
                    frame = json.loads(line)
                except ValueError:
                    continue
                if isinstance(frame, dict):
                    self.inbox.put(frame)
        finally:
            self.inbox.put(None)

    def call(self, method, params):
        self.next_request += 1
        id = f"agent-{self.next_request}"
        send({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        return id

    def pump(self, timeout):
        """Handles one incoming frame within `timeout` seconds; a prompt
        waiting on the client keeps answering everything else meanwhile."""
        try:
            frame = self.inbox.get(timeout=timeout)
        except queue.Empty:
            return True
        if frame is None:
            raise SystemExit
        self.dispatch(frame, in_turn=True)
        return True

    def wait_response(self, id, session, deadline=60):
        end = time.time() + deadline
        while id not in self.responses and time.time() < end:
            self.pump(0.1)
        return self.responses.pop(id, None)

    def pause(self, seconds, session):
        end = time.time() + seconds
        while time.time() < end and session.id not in self.cancelled:
            self.pump(max(0.0, min(0.05, end - time.time())))
        return session.id in self.cancelled

    def run(self):
        threading.Thread(target=self.read, daemon=True).start()
        while True:
            frame = self.inbox.get()
            if frame is None:
                return
            self.dispatch(frame, in_turn=False)

    def dispatch(self, frame, in_turn):
        method = frame.get("method")
        id = frame.get("id")
        params = frame.get("params") or {}
        if method is None:
            if id is not None:
                self.responses[id] = frame
            return
        if method == "session/cancel":
            self.cancelled.add(params.get("sessionId"))
            return
        if id is None:
            return
        handler = getattr(self, "on_" + method.replace("/", "_"), None)
        if handler is None:
            return fail(id, -32601, f"Method not found: {method}")
        if method == "session/prompt" and in_turn:
            return fail(id, -32600, "A prompt is already running")
        handler(id, params)

    def on_initialize(self, id, params):
        if os.environ.get("FAKE_ACP_NOISE"):
            noise("fake-acp-agent: starting up (this line is not JSON)")
        if AUTH == "initialize":
            return signed_out(id)
        load = not os.environ.get("FAKE_ACP_NO_LOAD")
        capabilities = {
            "loadSession": load,
            "promptCapabilities": {"image": True, "audio": False, "embeddedContext": True},
            "sessionCapabilities": {"list": {}, "resume": {}, "close": {}},
        }
        if SHAPE == "agy":
            reply(id, {"protocolVersion": 1, "agentCapabilities": capabilities,
                       "agentInfo": {"name": "Antigravity ACP Server", "version": "fixture"},
                       "authMethods": [{"id": "oauth-personal", "name": "Log in with Google"}]})
        else:
            reply(id, {"protocolVersion": 1, "agentCapabilities": capabilities,
                       "agentInfo": {"name": "muse", "version": "fixture"},
                       "authMethods": [{"id": "muse-login", "name": "Log in to Muse"}]})

    def on_session_new(self, id, params):
        if AUTH == "new" or logged_out():
            return signed_out(id)
        session = Session(str(uuid.uuid4()), params.get("cwd", ""))
        self.sessions[session.id] = session
        session.hold_presence()
        self.remember(session)
        reply(id, session.setup())
        session.update({"sessionUpdate": "available_commands_update",
                        "availableCommands": COMMANDS}, record=False)
        if SHAPE == "muse":
            session.title = MUSE_NAMES[int(session.id[:8], 16) % len(MUSE_NAMES)]
            self.remember(session)
            session.update({"sessionUpdate": "session_info_update", "title": session.title},
                           record=False)

    def on_session_load(self, id, params):
        if AUTH == "load" or logged_out():
            return signed_out(id)
        session = Session(params["sessionId"], params.get("cwd", ""))
        if not os.path.exists(session.path()):
            return fail(id, -32002, f"Session {session.id} not found")
        self.sessions[session.id] = session
        session.hold_presence()
        for index in range(int(os.environ.get("FAKE_ACP_REPLAY") or 0)):
            session.update({"sessionUpdate": "user_message_chunk",
                            "content": {"type": "text", "text": f"Earlier question {index}"}},
                           record=False)
            session.update({"sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": f"Earlier answer {index}"}},
                           record=False)
        with open(session.path()) as saved:
            for line in saved:
                session.update(json.loads(line), record=False)
        reply(id, session.setup(session_id=False))
        session.update({"sessionUpdate": "available_commands_update",
                        "availableCommands": COMMANDS}, record=False)

    def on_session_list(self, id, params):
        if AUTH == "list":
            return signed_out(id)
        index = self.index()
        sessions = [
            {"sessionId": sid, "cwd": entry["cwd"], "title": entry.get("title"),
             "updatedAt": entry["updatedAt"]}
            for sid, entry in sorted(index.items(), key=lambda kv: kv[1]["updatedAt"],
                                     reverse=True)
            if not params.get("cwd") or entry["cwd"] == params["cwd"]]
        page = int(os.environ.get("FAKE_ACP_LIST_PAGE") or 0)
        if page <= 0:
            return reply(id, {"sessions": sessions})
        start = int(params.get("cursor") or 0)
        result = {"sessions": sessions[start:start + page]}
        if start + page < len(sessions):
            result["nextCursor"] = str(start + page)
        reply(id, result)

    def on_session_close(self, id, params):
        session = self.sessions.pop(params.get("sessionId"), None)
        if session is not None:
            session.release_presence()
        reply(id, {})

    def on_session_set_config_option(self, id, params):
        session = self.sessions.get(params.get("sessionId"))
        if session is None:
            return fail(id, -32002, "Unknown session")
        option, value = params.get("configId"), params.get("value")
        known = {o["id"]: [c["value"] for c in o["options"]] for o in session.config_options()}
        if option not in known or value not in known[option]:
            return fail(id, -32602, f"Invalid value {value!r} for {option!r}")
        if option == "mode":
            session.mode = value
        elif option == "model":
            session.model = value
        else:
            session.level = value
        if SHAPE == "muse" and option == "model":
            # The options a model offers change with it.
            session.level = None
        reply(id, {"configOptions": session.config_options()})

    def on_session_set_mode(self, id, params):
        session = self.sessions.get(params.get("sessionId"))
        if session is None:
            return fail(id, -32002, "Unknown session")
        session.mode = params["modeId"]
        reply(id, {})
        session.update({"sessionUpdate": "current_mode_update", "currentModeId": session.mode},
                       record=False)

    def on_session_prompt(self, id, params):
        session = self.sessions.get(params.get("sessionId"))
        if session is None:
            return fail(id, -32002, "Unknown session")
        if AUTH == "prompt" or logged_out():
            return signed_out(id)
        self.cancelled.discard(session.id)
        blocks = params.get("prompt") or []
        text = " ".join(b.get("text", "") for b in blocks if b.get("type") == "text")
        for block in blocks:
            session.update({"sessionUpdate": "user_message_chunk", "content": block}, record=True)
        if session.title is None:
            session.title = text.strip().splitlines()[0][:60] if text.strip() else None
        self.remember(session)
        words = set(re.findall(r"[a-z]+", text.lower()))
        stop = "end_turn"
        if "auth" in words:
            return signed_out(id)
        if text.strip().startswith("/compact"):
            self.say(session, "Compacted the conversation.")
        elif "tools" in words:
            stop = self.tools(session)
        elif "edits" in words:
            stop = self.edits(session)
        elif "permission" in words:
            stop = self.permission(session)
        elif "hold" in words:
            stop = self.hold(session)
        elif "garbage" in words:
            self.say(session, "Before the stray line. ")
            noise("fake-acp-agent: a diagnostic line that is not JSON")
            self.say(session, "After the stray line.")
        elif "files" in words:
            asked = self.call("fs/read_text_file",
                              {"sessionId": session.id, "path": os.path.join(session.cwd, "README")})
            answer = self.wait_response(asked, session) or {}
            code = (answer.get("error") or {}).get("code")
            self.say(session, f"The client answered fs/read_text_file with error {code}.")
        elif "mode" in words:
            modes = [m[0] for m in session.modes()]
            session.mode = modes[(modes.index(session.mode) + 1) % len(modes)]
            session.update({"sessionUpdate": "current_mode_update",
                            "currentModeId": session.mode}, record=False)
            session.update({"sessionUpdate": "config_option_update",
                            "configOptions": session.config_options()}, record=False)
            self.say(session, f"Switched to {session.mode}.")
        elif "title" in words:
            session.title = "Fixture conversation title"
            self.remember(session)
            session.update({"sessionUpdate": "session_info_update", "title": session.title},
                           record=False)
            self.say(session, "Named the conversation.")
        elif "refuse" in words:
            self.say(session, "I will not do that.")
            stop = "refusal"
        else:
            self.think(session, "Considering the question.")
            self.say(session, "Fake answer to: " + text.strip().splitlines()[0][:200]
                     if text.strip() else "Fake answer.")
        if SHAPE == "muse":
            # Muse reports no context window size.
            session.update({"sessionUpdate": "usage_update", "used": 0, "size": 0,
                            "_meta": {"dev.muse": {"contextSizeUnknown": True}}}, record=False)
        else:
            # Antigravity reports its context window, never a cost.
            session.update({"sessionUpdate": "usage_update", "used": 1200, "size": 200000},
                           record=False)
        if session.id in self.cancelled:
            stop = "cancelled"
        result = {"stopReason": stop}
        if SHAPE == "agy":
            result["usage"] = {"totalTokens": 30, "inputTokens": 20, "outputTokens": 10}
        reply(id, result)

    def say(self, session, text, message=None):
        message = message or str(uuid.uuid4())
        for start in range(0, len(text), 12):
            session.update({"sessionUpdate": "agent_message_chunk", "messageId": message,
                            "content": {"type": "text", "text": text[start:start + 12]}})

    def think(self, session, text):
        session.update({"sessionUpdate": "agent_thought_chunk",
                        "content": {"type": "text", "text": text}})

    def tools(self, session):
        plan = [{"content": "Inspect the project", "priority": "high", "status": "in_progress"},
                {"content": "Edit the notes", "priority": "medium", "status": "pending"}]
        session.update({"sessionUpdate": "plan", "entries": plan})
        self.think(session, "Listing the files first.")
        call = "call-" + uuid.uuid4().hex[:8]
        session.update({"sessionUpdate": "tool_call", "toolCallId": call, "title": "List files",
                        "kind": "execute", "status": "pending",
                        "rawInput": {"command": "ls -1", "cwd": session.cwd}})
        session.update({"sessionUpdate": "tool_call_update", "toolCallId": call,
                        "status": "in_progress"})
        session.update({"sessionUpdate": "tool_call_update", "toolCallId": call,
                        "status": "completed", "rawOutput": {"exitCode": 0},
                        "content": [{"type": "content",
                                     "content": {"type": "text", "text": "README\nnotes.md"}}]})
        plan[0]["status"], plan[1]["status"] = "completed", "in_progress"
        session.update({"sessionUpdate": "plan", "entries": plan})
        edit = "call-" + uuid.uuid4().hex[:8]
        path = os.path.join(session.cwd, "notes.md")
        session.update({"sessionUpdate": "tool_call", "toolCallId": edit, "title": "Edit notes.md",
                        "kind": "edit", "status": "in_progress",
                        "locations": [{"path": path, "line": 1}]})
        session.update({"sessionUpdate": "tool_call_update", "toolCallId": edit,
                        "status": "completed",
                        "content": [{"type": "diff", "path": path, "oldText": "# Notes\nold line\n",
                                     "newText": "# Notes\nnew line\n"}]})
        plan[1]["status"] = "completed"
        session.update({"sessionUpdate": "plan", "entries": plan})
        self.say(session, "Listed the files and updated notes.md.")
        return "end_turn"

    def edits(self, session):
        """Edits shaped like a real change: several files in one call, then a
        long one, so a transcript shows multi-file, added, deleted and
        oversized patches."""
        def edit(title, diffs):
            call = "call-" + uuid.uuid4().hex[:8]
            session.update({"sessionUpdate": "tool_call", "toolCallId": call, "title": title,
                            "kind": "edit", "status": "in_progress",
                            "locations": [{"path": diffs[0]["path"], "line": 1}]})
            session.update({"sessionUpdate": "tool_call_update", "toolCallId": call,
                            "status": "completed", "content": diffs})
        here = lambda *parts: os.path.join(session.cwd, *parts)
        context = "".join(f"    let step_{n} = config.step({n});\n" for n in range(1, 13))
        before = ("use std::path::Path;\n\npub fn load(path: &Path) -> Config {\n"
                  "    let text = std::fs::read_to_string(path).unwrap();\n"
                  + context + "    Config::parse(&text)\n}\n")
        after = ("use std::path::Path;\n\npub fn load(path: &Path) -> Result<Config, Error> {\n"
                 "    let text = std::fs::read_to_string(path)?;\n"
                 + context + "    Config::parse(&text).map_err(Error::Parse)\n}\n")
        edit("Edit 3 files", [
            {"type": "diff", "path": here("src", "config.rs"), "oldText": before, "newText": after},
            {"type": "diff", "path": here("src", "error.rs"), "oldText": None,
             "newText": "#[derive(Debug)]\npub enum Error {\n    Io(std::io::Error),\n"
                        "    Parse(String),\n}\n"},
            {"type": "diff", "path": here("docs", "legacy-config.md"),
             "oldText": "# Legacy config\n\nThis format is no longer read.\n", "newText": ""},
        ])
        old = "".join(f"fn handler_{n}(request: &Request) -> Response {{\n    Response::ok()\n}}\n\n"
                      for n in range(40))
        new = "".join(f"fn handler_{n}(request: &Request) -> Response {{\n"
                      f"    tracing::debug!(id = {n}, \"handled\");\n    Response::ok()\n}}\n\n"
                      for n in range(40))
        edit("Edit handlers.rs", [{"type": "diff", "path": here("src", "handlers.rs"),
                                   "oldText": old, "newText": new}])
        self.say(session, "Made loading fallible and traced every handler.")
        return "end_turn"

    def permission(self, session):
        edit = "call-" + uuid.uuid4().hex[:8]
        path = os.path.join(session.cwd, "config.toml")
        diff = [{"type": "diff", "path": path, "oldText": "debug = false\n",
                 "newText": "debug = true\n"}]
        session.update({"sessionUpdate": "tool_call", "toolCallId": edit,
                        "title": "Edit config.toml", "kind": "edit", "status": "pending",
                        "content": diff, "locations": [{"path": path}]})
        asked = self.call("session/request_permission", {
            "sessionId": session.id,
            "toolCall": {"toolCallId": edit, "title": "Edit config.toml", "kind": "edit",
                         "status": "pending", "content": diff},
            "options": [
                {"optionId": "proceed_once", "name": "Allow", "kind": "allow_once"},
                {"optionId": "proceed_always", "name": "Always allow", "kind": "allow_always"},
                {"optionId": "cancel", "name": "Reject", "kind": "reject_once"},
            ]})
        answer = self.wait_response(asked, session)
        outcome = ((answer or {}).get("result") or {}).get("outcome") or {}
        if outcome.get("outcome") == "selected" and outcome.get("optionId", "").startswith("proceed"):
            session.update({"sessionUpdate": "tool_call_update", "toolCallId": edit,
                            "status": "completed"})
            self.say(session, "Turned on debugging in config.toml.")
            return "end_turn"
        if outcome.get("outcome") == "cancelled" or session.id in self.cancelled:
            session.update({"sessionUpdate": "tool_call_update", "toolCallId": edit,
                            "status": "failed"})
            return "cancelled"
        session.update({"sessionUpdate": "tool_call_update", "toolCallId": edit,
                        "status": "failed",
                        "content": [{"type": "content",
                                     "content": {"type": "text", "text": "Refused by the user"}}]})
        self.say(session, "Left config.toml unchanged.")
        return "end_turn"

    def hold(self, session):
        call = "call-" + uuid.uuid4().hex[:8]
        session.update({"sessionUpdate": "tool_call", "toolCallId": call,
                        "title": "Wait for a long build", "kind": "execute",
                        "status": "in_progress", "rawInput": {"command": "sleep 60"}})
        for _ in range(300):
            self.think(session, "Still working. ")
            if self.pause(0.2, session):
                return "cancelled"
        self.say(session, "Finished holding.")
        return "end_turn"

    def index(self):
        try:
            with open(os.path.join(STATE, "index.json")) as saved:
                return json.load(saved)
        except (OSError, ValueError):
            return {}

    def remember(self, session):
        index = self.index()
        index[session.id] = {"cwd": session.cwd, "title": session.title,
                             "updatedAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
        os.makedirs(STATE, exist_ok=True)
        with open(os.path.join(STATE, "index.json.tmp"), "w") as saved:
            json.dump(index, saved)
        os.replace(os.path.join(STATE, "index.json.tmp"), os.path.join(STATE, "index.json"))


def command_line(arguments):
    """Muse's account commands, for a fixture installed as `muse-upstream`:
    `usage --json`, `login` and `logout`. Anything else serves ACP."""
    if SHAPE != "muse" or not arguments or arguments[0] not in ("usage", "login", "logout"):
        return None
    marker = os.path.join(STATE, "signed-out")
    os.makedirs(STATE, exist_ok=True)
    if arguments[0] == "logout":
        open(marker, "w").close()
        print("Logged out of Muse.")
        return 0
    if arguments[0] == "login":
        print("To log in, open https://example.invalid/device and enter the code FAKE-CODE.")
        sys.stdout.flush()
        time.sleep(float(os.environ.get("FAKE_ACP_LOGIN_SECONDS") or 2))
        if os.path.exists(marker):
            os.remove(marker)
        print("Logged in to Muse.")
        return 0
    if AUTH or os.path.exists(marker):
        print("muse usage: not logged in: run `muse login` first", file=sys.stderr)
        return 1
    if "--json" in arguments:
        print(json.dumps({"allowance": None, "hint": None, "reason": "no_allowance",
                          "schemaVersion": 1}))
    else:
        print("No subscription allowance.")
    return 0


if __name__ == "__main__":
    status = command_line(sys.argv[1:])
    if status is not None:
        sys.exit(status)
    try:
        Agent().run()
    except (SystemExit, BrokenPipeError, KeyboardInterrupt):
        pass
