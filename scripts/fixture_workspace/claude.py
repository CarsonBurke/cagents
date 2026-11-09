"""Claude Code transcripts, as Claude Code 2.x writes them.

A transcript is CONFIG/projects/<slug of cwd>/<sessionId>.jsonl: one JSON
record per line, chained by parentUuid. Subagents keep their own records in
<sessionId>/subagents/agent-<agentId>.jsonl beside a small .meta.json.

To add a record kind, add a method to `Conversation` that appends it, and call
it from `turn` (or from `write` for a once-per-conversation record).
"""

import hashlib
import json
import os

from . import content
from .determinism import iso

VERSION = "2.1.90"
MODEL = "claude-sonnet-4-5"


def slug(path):
    """Claude Code's project folder name for a working directory."""
    sanitized = "".join(c if c.isascii() and c.isalnum() else "-" for c in path)
    if len(sanitized) <= 200:
        return sanitized
    # Past 200 characters Claude Code keeps a prefix and appends a hash. The
    # reader matches the prefix only, so any stable hash serves.
    return f"{sanitized[:200]}-{int(hashlib.sha256(path.encode()).hexdigest()[:10], 16):x}"


def transcript_path(home, cwd, file_id):
    return os.path.join(home, ".claude", "projects", slug(cwd), f"{file_id}.jsonl")


class Conversation:
    """Builds one session's records in order."""

    def __init__(self, stream, session_id, cwd, branch, started_at):
        self.stream = stream
        self.session_id = session_id
        self.cwd = cwd
        self.branch = branch
        self.now = started_at
        self.parent = None
        self.lines = []
        self.sidecars = []  # (agent_id, meta, lines)
        self.shown = []  # uuids of the records the reader shows as messages.
        self.sidechain = False

    def tick(self, low=2, high=40):
        self.now += self.stream.uniform(low, high)

    def emit(self, record, chain=True):
        self.lines.append(json.dumps(record, separators=(",", ":"), ensure_ascii=False))
        if chain and "uuid" in record:
            self.parent = record["uuid"]

    def base(self, kind, **fields):
        record = {
            "parentUuid": self.parent,
            "isSidechain": self.sidechain,
            "userType": "external",
            "cwd": self.cwd,
            "sessionId": self.session_id,
            "version": VERSION,
            "gitBranch": self.branch,
            "type": kind,
            **fields,
            "uuid": self.stream.uuid4(),
            "timestamp": iso(self.now),
        }
        return record

    # Records.

    def user_text(self, text, **fields):
        self.tick()
        self.emit(self.base("user", message={"role": "user", "content": text}, **fields))
        if not fields.get("isMeta") and not self.sidechain:
            self.shown.append(self.parent)

    def noise(self, name):
        fields, text = content.NOISE[name]
        self.tick(0, 2)
        if name == "compact-summary":
            self.emit(self.base("system", subtype="compact_boundary", content="Conversation compacted",
                                isMeta=False, level="info",
                                compactMetadata={"trigger": "auto", "preTokens": 167000}))
        self.emit(self.base("user", message={"role": "user", "content": text}, **fields))

    def assistant(self, block, message_id, usage=True):
        self.tick(1, 20)
        message = {"id": message_id, "type": "message", "role": "assistant", "model": MODEL,
                   "content": [block], "stop_reason": None, "stop_sequence": None}
        if usage:
            message["usage"] = {"input_tokens": self.stream.randint(3, 40),
                                "cache_read_input_tokens": self.stream.randint(10_000, 90_000),
                                "output_tokens": self.stream.randint(20, 4000)}
        self.emit(self.base("assistant", message=message, requestId="req_" + self.stream.token(24)))
        if block["type"] == "text" and not self.sidechain:
            self.shown.append(self.parent)

    def tool(self, message_id, name, arguments, result, tool_result):
        call = "toolu_" + self.stream.token(24)
        self.assistant({"type": "tool_use", "id": call, "name": name, "input": arguments}, message_id)
        source = self.parent
        self.tick(1, 30)
        self.emit(self.base("user", message={"role": "user", "content": [
            {"tool_use_id": call, "type": "tool_result", "content": result, "is_error": False}]},
            toolUseResult=tool_result, sourceToolAssistantUUID=source))
        return call

    def bash(self, message_id):
        command, output = self.stream.choice(content.COMMANDS)
        self.tool(message_id, "Bash", {"command": command, "description": "Run " + command.split()[0]},
                  output, {"stdout": output, "stderr": "", "interrupted": False, "isImage": False})

    def read(self, message_id):
        path = os.path.join(self.cwd, self.stream.choice(content.FILES))
        text = content.CODE[self.stream.choice(sorted(content.CODE))].format(name="shown")
        numbered = "\n".join(f"{n:>6}\t{line}" for n, line in enumerate(text.splitlines(), 1))
        self.tool(message_id, "Read", {"file_path": path}, numbered,
                  {"type": "text", "file": {"filePath": path, "content": text, "numLines": text.count("\n"),
                                            "startLine": 1, "totalLines": text.count("\n")}})

    def edit(self, message_id):
        path = os.path.join(self.cwd, self.stream.choice(content.FILES))
        old, new = "let token = store.refresh_token(user)?;", "let token = guard.refresh_token(user)?;"
        self.tool(message_id, "Edit", {"file_path": path, "old_string": old, "new_string": new},
                  f"The file {path} has been updated successfully.",
                  {"filePath": path, "oldString": old, "newString": new, "replaceAll": False,
                   "structuredPatch": [{"oldStart": 41, "oldLines": 1, "newStart": 41, "newLines": 1,
                                        "lines": [f"-{old}", f"+{new}"]}], "userModified": False})

    def agent(self, message_id, background):
        """An Agent call, with the subagent's own records in a sidecar."""
        description, prompt = self.stream.choice(content.SUBAGENT_TASKS)
        agent_id = self.stream.hex(17)
        arguments = {"description": description, "prompt": prompt, "subagent_type": "general-purpose"}
        if background:
            arguments["run_in_background"] = True
        report = self.stream.choice(content.REPLIES)
        if background:
            call = self.tool(message_id, "Agent", arguments, "Async agent launched successfully.",
                             {"isAsync": True, "status": "async_launched", "agentId": agent_id,
                              "description": description, "prompt": prompt})
        else:
            call = self.tool(message_id, "Agent", arguments, [{"type": "text", "text": report}],
                             {"status": "completed", "prompt": prompt, "agentId": agent_id,
                              "content": [{"type": "text", "text": report}],
                              "totalDurationMs": self.stream.randint(8000, 400000),
                              "totalTokens": self.stream.randint(5000, 90000),
                              "totalToolUseCount": self.stream.randint(1, 40)})
        child = Conversation(self.stream, self.session_id, self.cwd, self.branch, self.now)
        child.sidechain = True
        child.user_text(prompt)
        child.bash("msg_" + self.stream.token(24))
        child.assistant({"type": "text", "text": report}, "msg_" + self.stream.token(24))
        lines = []
        for line in child.lines:
            record = json.loads(line)
            record["agentId"] = agent_id
            lines.append(json.dumps(record, separators=(",", ":"), ensure_ascii=False))
        self.sidecars.append((agent_id, {"toolUseId": call, "agentType": "general-purpose",
                                         "description": description}, lines))
        if background:
            notification = (f"<task-notification>\n<task-id>{agent_id}</task-id>\n<tool-use-id>{call}</tool-use-id>\n"
                            f"<status>completed</status>\n<summary>Agent \"{description}\" completed</summary>\n"
                            f"<result>{report}</result>\n</task-notification>")
            self.tick(30, 300)
            self.emit({"type": "queue-operation", "operation": "enqueue", "timestamp": iso(self.now),
                       "sessionId": self.session_id, "content": notification}, chain=False)
            self.emit(self.base("user", message={"role": "user", "content": notification},
                                origin={"kind": "task-notification"}))

    def turn(self, prompt, long, subagent):
        self.user_text(prompt)
        message_id = "msg_" + self.stream.token(24)
        if self.stream.random() < 0.6:
            self.assistant({"type": "thinking", "thinking": self.stream.choice(content.THOUGHTS),
                            "signature": self.stream.token(88)}, message_id)
        tools = self.stream.randint(2, 7) if long else self.stream.randint(0, 2)
        for _ in range(tools):
            self.stream.choice((self.bash, self.read, self.edit))(message_id)
        if subagent:
            self.agent(message_id, background=self.stream.random() < 0.5)
        reply = content.long_reply(self.stream, self.stream.randint(2, 5)) if long else self.stream.choice(content.REPLIES)
        self.assistant({"type": "text", "text": reply}, "msg_" + self.stream.token(24))
        self.emit({"type": "last-prompt", "lastPrompt": prompt, "sessionId": self.session_id}, chain=False)

    def title(self, kind, text):
        field = {"ai-title": "aiTitle", "custom-title": "customTitle"}[kind]
        self.emit({"type": kind, "sessionId": self.session_id, field: text}, chain=False)

    def large_output(self, megabytes):
        """One intact tool result of `megabytes` MiB, a build log nobody
        trimmed, and the answer that follows on from it."""
        log = ("log line " + "x" * 90 + "\n") * (megabytes * 1024 * 1024 // 100 + 1)
        message_id = "msg_" + self.stream.token(24)
        self.tool(message_id, "Bash", {"command": "make -j16 V=1 2>&1", "description": "Verbose build"}, log,
                  {"stdout": log[:4096], "stderr": "", "interrupted": False, "isImage": False})
        record = self.parent
        self.assistant({"type": "text", "text": "The build log is long, but the only failure is the linker "
                                                "warning at the end; the build itself succeeded."}, message_id)
        return record

    def pad(self, megabytes):
        """Large tool output, as long agent sessions accumulate, one MiB per
        record so no single record approaches the reader's record limit."""
        chunk = ("log line " + "x" * 90 + "\n") * (1024 * 1024 // 100)
        message_id = "msg_" + self.stream.token(24)
        for _ in range(megabytes):
            self.tool(message_id, "Bash", {"command": "make -j16 2>&1", "description": "Build"}, chunk,
                      {"stdout": chunk, "stderr": "", "interrupted": False, "isImage": False})


def chain_record(line):
    """The record on `line` if it is part of the parentUuid chain."""
    record = json.loads(line)
    return record if "uuid" in record and record.get("type") in ("user", "assistant") else None


def write(home, spec, stream):
    """Writes one conversation the workspace plan describes. Returns what a
    reader should find in it."""
    conversation = Conversation(stream, spec["session_id"], spec["cwd"], spec["branch"], spec["started_at"])
    edge = spec.get("edge")
    prefix = []
    shown_before = []
    details = {}
    if edge == "forked":
        # As `claude --resume ID --fork-session` writes it (Claude Code
        # 2.1.296): the origin's messages with their own uuids, rewritten
        # under the fork's sessionId, then the fork's own turns.
        copied = [record for record in map(chain_record, spec["origin_lines"]) if record]
        prefix = [json.dumps({**record, "sessionId": spec["session_id"]}, separators=(",", ":"), ensure_ascii=False)
                  for record in copied]
        shown_before = list(spec["origin_shown"])
        conversation.parent = copied[-1]["uuid"] if copied else None
    for name in spec.get("noise", ()):
        conversation.noise(name)
    for index, prompt in enumerate(spec["prompts"]):
        conversation.turn(prompt, long=index in spec["long_turns"], subagent=index in spec["subagent_turns"])
        if edge == "sidechain" and index == 0:
            # A branch the main chain does not continue from.
            parent = conversation.parent
            conversation.sidechain = True
            conversation.user_text("Warm up the cache before the real request.")
            conversation.assistant({"type": "text", "text": "Ready."}, "msg_" + stream.token(24))
            conversation.sidechain = False
            conversation.parent = parent
        if edge == "large-record" and index == 0:
            details["record"] = conversation.large_output(spec["large_mib"])
    if spec.get("pad_mib"):
        conversation.pad(spec["pad_mib"])
    if spec.get("title"):
        conversation.title(*spec["title"])
    lines = prefix + conversation.lines
    shown = shown_before + conversation.shown
    tail = "\n"
    if edge == "malformed-record":
        lines.insert(len(lines) // 2, "this line is not JSON {")
    elif edge == "truncated-record":
        # What a crash mid-save leaves once Claude Code goes on writing: a
        # message cut short, with the next message appended to the same line
        # and following on from the message before the cut one.
        records = [chain_record(line) for line in lines]
        cut = next(index for index in range(len(lines) // 2, len(lines) - 1)
                   if records[index] and records[index + 1])
        lost, joined = records[cut], records[cut + 1]
        joined["parentUuid"] = lost["parentUuid"]
        lines[cut + 1] = lines[cut][: len(lines[cut]) // 3] + json.dumps(joined, separators=(",", ":"),
                                                                         ensure_ascii=False)
        del lines[cut]
        # The joined record is intact but shares the damaged line, so whether
        # it shows is the reader's call; the lost one never can.
        shown = [uuid for uuid in shown if uuid not in (lost["uuid"], joined["uuid"])]
        details = {"lost": lost["uuid"], "joined": joined["uuid"]}
    elif edge == "unfinished-line":
        lines[-1] = lines[-1][: len(lines[-1]) // 2]
        tail = ""
    path = transcript_path(home, spec["cwd"], spec["file_id"])
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as file:
        file.write("\n".join(lines) + tail)
    for agent_id, meta, child in conversation.sidecars:
        directory = os.path.join(os.path.dirname(path), spec["file_id"], "subagents")
        os.makedirs(directory, exist_ok=True)
        records = os.path.join(directory, f"agent-{agent_id}.jsonl")
        with open(records, "w") as file:
            file.write("\n".join(child) + "\n")
        with open(os.path.join(directory, f"agent-{agent_id}.meta.json"), "w") as file:
            json.dump(meta, file)
        for sidecar in (records, os.path.join(directory, f"agent-{agent_id}.meta.json")):
            os.utime(sidecar, (conversation.now, conversation.now))
    os.utime(path, (conversation.now, conversation.now))
    return {"path": path, "lines": lines, "shown": shown, "visible": len(shown),
            "subagents": len(conversation.sidecars), "ended_at": conversation.now, "details": details}
