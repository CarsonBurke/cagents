#!/usr/bin/python3
"""Offline Agent Client Protocol (v1) agent for cagents' ACP chat adapter.

It answers over stdio like `agy acp` or `muse acp`, without network access,
credentials or a model, and never touches the real agents' data. Its replies
copy the shapes of Antigravity 1.5.12's and Muse 1.4.23's `initialize` and
`session/new` answers: by default Antigravity's, Muse's when it is run as
`muse` or `muse-upstream` or with FAKE_ACP_SHAPE=muse. Any command-line
arguments, such as `acp --disable-telemetry`, are accepted and ignored.

Each conversation is saved under FAKE_ACP_STATE (default: a directory in
the system's temporary directory) as the session updates it sent, so
`session/load` replays it and `session/list` lists it.

The prompt chooses the turn, by the first of these words it contains as a
word:

  tools       a plan, a shell command, a file edit with its diff, then an answer
  permission  an edit that asks for approval first; refusing fails the edit,
              stopping the turn cancels it
  hold        thinks until the turn is stopped (at most 60 seconds)
  garbage     prints a line that is not JSON in the middle of its answer
  files       asks the client to read a file and reports what it was told
  mode        switches its own mode and reports its new config options
  title       names the conversation
  refuse      ends the turn with the `refusal` stop reason
  auth        refuses the prompt until the user signs in
  allowance   refuses the prompt as a login with no usage allowance
  /compact    answers as a compaction command

Anything else is answered with a short thought and a streamed reply.

Environment switches:
  FAKE_ACP_SHAPE=agy|muse   which agent's replies to copy
  FAKE_ACP_STATE=DIR        where conversations are saved
  FAKE_ACP_AUTH=initialize|new|load|prompt
                            refuse that request as signed out (-32000)
  FAKE_ACP_NO_LOAD=1        do not offer session/load
  FAKE_ACP_NOISE=1          print a line that is not JSON before answering
                            `initialize`
  FAKE_ACP_REPLAY=N         a loaded conversation starts with N extra
                            exchanges, for long histories
  FAKE_ACP_LOG=FILE         append every frame received, one per line
  FAKE_ACP_LOGIN_SECONDS=N  how long `login` waits before signing in
  FAKE_ACP_AUTH_FILE=FILE   the agent's sign-in record: `login` writes it and
                            `logout` removes it, as Muse's auth.json. Point it
                            only at a fixture's own file.

Files under FAKE_ACP_STATE switch states that last across processes:
  signed-out      every new or loaded session and every prompt is refused as
                  signed out; Muse's `logout` writes it and `login` removes it
  no-allowance    every prompt is refused as a login with no usage allowance,
                  and Muse's `usage --json` reports no allowance
  hang-probe      a session opened by a probe (`--app-data-dir`) never answers

Run with `--app-data-dir DIR`, as cagents' Antigravity probe runs `agy acp`,
it saves each new session's files under ~/.gemini/DIR the way agy does
(`conversations/ID.db`, `brain/ID/`, `annotations/ID.pbtxt` and
`implicit/ID.pb`), so a probe can be checked for leaving them behind.

In the Muse shape, `usage [--json]`, `login` and `logout` answer like Muse's
account commands.
"""
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
AUTH = os.environ.get("FAKE_ACP_AUTH", "")
LOG = os.environ.get("FAKE_ACP_LOG")


def app_data_dir(arguments):
    """Where `--app-data-dir DIR` puts the agent's data: like agy, relative
    to ~/.gemini."""
    for index, argument in enumerate(arguments):
        if argument == "--app-data-dir" and index + 1 < len(arguments):
            return os.path.join(os.path.expanduser("~/.gemini"), arguments[index + 1])
        if argument.startswith("--app-data-dir="):
            return os.path.join(os.path.expanduser("~/.gemini"), argument.split("=", 1)[1])
    return None


APP_DATA = app_data_dir(sys.argv[1:])

AGY_MODES = [
    ("default", "Default", "Ask before edits and commands, per your permission settings."),
    ("accept-edits", "Accept edits", "Approve file edits in the workspace without asking."),
    ("auto", "Auto", "Approve edits and safe commands; ask before irreversible actions."),
    ("plan", "Plan", "Plan carefully before executing a task."),
]
AGY_LEVELS = [("low", "Low"), ("medium", "Medium"), ("high", "High"), ("xhigh", "Extra")]
# As Antigravity 1.5.12 offers them: each model with the levels it has, and
# one, whose identifier ends like a level, with none.
AGY_MODELS = [
    ("gemini-3.8-flash", "Gemini 3.8 Flash", AGY_LEVELS[:3]),
    ("gemini-4-neon", "Gemini 4 Neon", AGY_LEVELS),
    ("gpt-oss-120b-medium", "GPT-OSS 120B (Medium)", []),
]
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
    return marked("signed-out")


def marked(name):
    return os.path.exists(os.path.join(STATE, name))


def no_allowance(id):
    """A prompt refused as Muse refuses one from a login with no subscription
    allowance. Muse's own reply was not captured: its service answered 402
    Payment Required, and this carries that reason in the error's data the
    way ACP agents report a failed model request."""
    send({"jsonrpc": "2.0", "id": id, "error": {
        "code": -32603, "message": "Internal error",
        "data": {"message": "model request failed: 402 Payment Required: "
                            "this login has no subscription allowance"}}})


class Session:
    def __init__(self, id, cwd):
        self.id = id
        self.cwd = cwd
        if SHAPE == "muse":
            self.mode, self.model, self.level = "onRequest", "muse-spark-1.2", None
        else:
            self.mode, self.model, self.level = "default", "gemini-3.8-flash", "low"
        self.title = None

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
        models = [(v, n) for v, n, _ in AGY_MODELS] if SHAPE == "agy" else MUSE_MODELS
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
        # Like agy, the level option lists the current model's levels only.
        levels = self.agy_levels()
        if not levels:
            return [model, mode]
        level = {"id": "thought_level", "name": "Thinking Level", "category": "thought_level",
                 "type": "select", "currentValue": self.level,
                 "options": [{"value": v, "name": n} for v, n in levels]}
        return [model, level, mode]

    def agy_levels(self):
        return next((levels for v, _, levels in AGY_MODELS if v == self.model), [])

    def setup(self, session_id=True):
        result = {
            "modes": {"currentModeId": self.mode,
                      "availableModes": [{"id": v, "name": n, "description": d}
                                         for v, n, d in self.modes()]},
            "configOptions": self.config_options(),
        }
        if SHAPE == "agy":
            result["models"] = {
                # One identifier per model and level, or the model's own.
                "availableModels": [
                    model
                    for v, n, levels in AGY_MODELS
                    for model in ([{"modelId": f"{v}-{level}", "name": f"{n} ({ln})"}
                                   for level, ln in levels] or [{"modelId": v, "name": n}])],
                "currentModelId": f"{self.model}-{self.level}" if self.level else self.model,
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
        self.remember(session)
        if APP_DATA:
            # What agy keeps of every session it opens, a probe's included.
            os.makedirs(os.path.join(APP_DATA, "brain", session.id), exist_ok=True)
            os.makedirs(os.path.join(APP_DATA, "conversations"), exist_ok=True)
            open(os.path.join(APP_DATA, "conversations", session.id + ".db"), "w").close()
            for store, suffix in (("annotations", ".pbtxt"), ("implicit", ".pb")):
                os.makedirs(os.path.join(APP_DATA, store), exist_ok=True)
                open(os.path.join(APP_DATA, store, session.id + suffix), "w").close()
            if marked("hang-probe"):
                # A probe that never hears back, as one that timed out.
                while self.inbox.get() is not None:
                    pass
                raise SystemExit
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
        index = self.index()
        reply(id, {"sessions": [
            {"sessionId": sid, "cwd": entry["cwd"], "title": entry.get("title"),
             "updatedAt": entry["updatedAt"]}
            for sid, entry in sorted(index.items(), key=lambda kv: kv[1]["updatedAt"],
                                     reverse=True)
            if not params.get("cwd") or entry["cwd"] == params["cwd"]]})

    def on_session_close(self, id, params):
        self.sessions.pop(params.get("sessionId"), None)
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
        elif option == "model":
            # A level the new model lacks becomes one it has.
            levels = [v for v, _ in session.agy_levels()]
            if session.level not in levels:
                session.level = ("medium" if "medium" in levels else
                                 levels[0] if levels else None)
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
        if marked("no-allowance"):
            return no_allowance(id)
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
        if "allowance" in words:
            return no_allowance(id)
        if text.strip().startswith("/compact"):
            self.say(session, "Compacted the conversation.")
        elif "tools" in words:
            stop = self.tools(session)
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
            session.update({"sessionUpdate": "usage_update", "used": 1200, "size": 200000,
                            "cost": {"amount": 0.0012, "currency": "USD"}}, record=False)
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
    record = os.environ.get("FAKE_ACP_AUTH_FILE")
    os.makedirs(STATE, exist_ok=True)
    if arguments[0] == "logout":
        open(marker, "w").close()
        if record and os.path.exists(record):
            os.remove(record)
        print("Logged out of Muse.")
        return 0
    if arguments[0] == "login":
        print("To log in, open https://example.invalid/device and enter the code FAKE-CODE.")
        sys.stdout.flush()
        time.sleep(float(os.environ.get("FAKE_ACP_LOGIN_SECONDS") or 2))
        if os.path.exists(marker):
            os.remove(marker)
        if record:
            os.makedirs(os.path.dirname(record), exist_ok=True)
            with open(record, "w") as saved:
                json.dump({"fixture": True, "signedInAt": time.time()}, saved)
        print("Logged in to Muse.")
        return 0
    if AUTH or os.path.exists(marker):
        print("muse usage: not logged in: run `muse login` first", file=sys.stderr)
        return 1
    if marked("no-allowance"):
        report = {"allowance": None, "hint": None, "reason": "no_allowance", "schemaVersion": 1}
        text = "No subscription allowance."
    else:
        # The fields Muse 1.4.24's report names; their meaning is not
        # published, so cagents reads only that an allowance is there.
        resets = int((time.time() + 3 * 3600) * 1000)
        report = {"allowance": {"usedPercent": 12.0, "resetsAtMs": resets,
                                "asOfMs": int(time.time() * 1000)},
                  "hint": None, "reason": None, "schemaVersion": 1}
        text = "Subscription allowance: 12% used."
    print(json.dumps(report) if "--json" in arguments else text)
    return 0


if __name__ == "__main__":
    status = command_line(sys.argv[1:])
    if status is not None:
        sys.exit(status)
    try:
        Agent().run()
    except (SystemExit, BrokenPipeError, KeyboardInterrupt):
        pass
