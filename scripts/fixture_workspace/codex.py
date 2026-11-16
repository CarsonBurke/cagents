"""Codex rollouts, as codex 0.166 writes them.

A rollout is CODEX_HOME/sessions/YYYY/MM/DD/rollout-<start>-<threadId>.jsonl.
Names a person gave a thread live in CODEX_HOME/session_index.jsonl.

Each turn must be framed exactly as codex frames it: event_msg task_started
(with the turn ID), turn_context, the user message, the work, the assistant
message, then event_msg task_complete. codex's thread/resume and thread/read
count turns by task_started; without it a thread has no turns.

To add a record kind, add a method to `Rollout` and call it from `turn`.
"""

import fcntl
import json
import os

from . import content
from .determinism import day_path, file_stamp, iso

CLI_VERSION = "0.166.0"
MODEL = "gpt-5.2-codex"
CONTEXT_WINDOW = 258400


class Rollout:
    def __init__(self, stream, thread_id, cwd, branch, started_at):
        self.stream = stream
        self.thread_id = thread_id
        self.cwd = cwd
        self.branch = branch
        self.now = started_at
        self.lines = []
        self.turns = 0

    def emit(self, kind, payload):
        self.lines.append(json.dumps({"timestamp": iso(self.now), "type": kind, "payload": payload},
                                     separators=(",", ":"), ensure_ascii=False))

    def event(self, payload):
        self.emit("event_msg", payload)

    def session_meta(self, parent=None, nickname=None):
        if parent:
            # codex refuses to read a spawned thread whose thread_spawn lacks
            # any of these ("failed to read session metadata").
            source = {"subagent": {"thread_spawn": {
                "parent_thread_id": parent, "depth": 1, "agent_path": f"/root/{nickname.lower()}",
                "agent_nickname": nickname, "agent_role": "default"}}}
        else:
            source = "cli"
        self.emit("session_meta", {
            "id": self.thread_id, "timestamp": iso(self.now), "cwd": self.cwd, "originator": "codex_cli_rs",
            "cli_version": CLI_VERSION, "source": source, "model_provider": "openai",
            "git": {"commit_hash": self.stream.hex(40), "branch": self.branch}})

    def message(self, role, text):
        kind = "input_text" if role == "user" else "output_text"
        payload = {"type": "message", "role": role, "content": [{"type": kind, "text": text}]}
        if role == "assistant":
            payload["id"] = "msg_" + self.stream.hex(32)
            payload["phase"] = "final_answer"
        self.emit("response_item", payload)

    def harness_context(self):
        """What codex sends before the first request; never a user message."""
        self.message("user", content.AGENTS_MD.format(cwd=self.cwd))
        self.message("user", content.ENVIRONMENT_CONTEXT.format(cwd=self.cwd))

    def completed(self, turn, item, started_ms):
        self.event({"type": "item_completed", "thread_id": self.thread_id, "turn_id": turn, "item": item,
                    "started_at_ms": started_ms, "completed_at_ms": int(self.now * 1000)})

    def command(self, turn):
        command, output = self.stream.choice(content.COMMANDS)
        call = "call_" + self.stream.token(24)
        started_ms = int(self.now * 1000)
        self.emit("response_item", {"type": "function_call", "name": "exec_command", "call_id": call,
                                    "arguments": json.dumps({"cmd": command, "workdir": self.cwd})})
        self.now += self.stream.uniform(1, 40)
        self.emit("response_item", {"type": "function_call_output", "call_id": call,
                                    "output": f"Process exited with code 0\nOutput:\n{output}"})
        self.completed(turn, {"type": "CommandExecution", "id": call, "command": ["/usr/bin/bash", "-lc", command],
                              "cwd": self.cwd, "status": "completed", "exit_code": 0,
                              "aggregated_output": output}, started_ms)

    def turn(self, prompt, long, ending):
        """One user turn. `ending` is "complete", "interrupted" or "usage-limit"."""
        self.turns += 1
        turn = self.stream.uuid4()
        self.now += self.stream.uniform(20, 600)
        started = int(self.now)
        self.event({"type": "task_started",
                    "turn_attribution": {"turn_id": turn, "turn_trigger": None, "parent_turn_id": None,
                                         "initiating_agent_path": None, "root_turn_id": turn},
                    "turn_id": turn, "root_turn_id": turn, "started_at": started,
                    "model_context_window": CONTEXT_WINDOW, "collaboration_mode_kind": "default"})
        self.emit("turn_context", {"turn_id": turn, "cwd": self.cwd, "approval_policy": "on-request",
                                   "sandbox_policy": {"type": "workspace-write"}, "model": MODEL,
                                   "effort": "medium", "summary": "auto"})
        self.message("user", prompt)
        self.event({"type": "user_message", "message": prompt, "images": [], "local_images": [],
                    "text_elements": []})
        self.completed(turn, {"type": "UserMessage", "id": self.stream.uuid4(),
                              "content": [{"type": "text", "text": prompt, "text_elements": []}]}, started * 1000)
        for _ in range(self.stream.randint(2, 6) if long else self.stream.randint(0, 2)):
            self.command(turn)
        if ending == "interrupted":
            self.now += self.stream.uniform(5, 60)
            self.event({"type": "turn_aborted", "turn_id": turn, "root_turn_id": turn, "reason": "interrupted",
                        "started_at": started})
            return
        if ending == "usage-limit":
            self.now += self.stream.uniform(1, 5)
            message = "You've hit your usage limit. Try again later."
            self.event({"type": "error", "message": message, "codex_error_info": "usage_limit_exceeded"})
            self.event({"type": "task_complete", "root_turn_id": turn, "turn_id": turn, "last_agent_message": None,
                        "started_at": started, "completed_at": int(self.now),
                        "duration_ms": (int(self.now) - started) * 1000,
                        "error": {"message": message, "codex_error_info": "usage_limit_exceeded"}})
            return
        reply = content.long_reply(self.stream, self.stream.randint(1, 4)) if long else self.stream.choice(content.REPLIES)
        reply_started = int(self.now * 1000)
        self.now += self.stream.uniform(5, 200)
        self.message("assistant", reply)
        self.event({"type": "agent_message", "message": reply, "phase": "final_answer"})
        self.completed(turn, {"type": "AgentMessage", "id": "msg_" + self.stream.hex(32),
                              "content": [{"type": "Text", "text": reply}], "phase": "final_answer"}, reply_started)
        self.event({"type": "token_count", "info": {"total_token_usage": {
            "input_tokens": self.stream.randint(5000, 200000), "output_tokens": self.stream.randint(100, 9000)}}})
        self.event({"type": "task_complete", "root_turn_id": turn, "turn_id": turn, "last_agent_message": reply,
                    "started_at": started, "completed_at": int(self.now),
                    "duration_ms": (int(self.now) - started) * 1000})


def path(codex_home, thread_id, started_at, archived=False):
    name = f"rollout-{file_stamp(started_at)}-{thread_id}.jsonl"
    if archived:
        return os.path.join(codex_home, "archived_sessions", name)
    return os.path.join(codex_home, "sessions", day_path(started_at), name)


def write(codex_home, spec, stream):
    rollout = Rollout(stream, spec["thread_id"], spec["cwd"], spec["branch"], spec["started_at"])
    rollout.session_meta(spec.get("parent"), spec.get("nickname"))
    rollout.harness_context()
    for index, prompt in enumerate(spec["prompts"]):
        rollout.turn(prompt, long=index in spec["long_turns"], ending=spec["endings"].get(index, "complete"))
    target = path(codex_home, spec["thread_id"], spec["started_at"], spec.get("archived", False))
    os.makedirs(os.path.dirname(target), exist_ok=True)
    with open(target, "w") as file:
        file.write("\n".join(rollout.lines) + "\n")
    os.utime(target, (rollout.now, rollout.now))
    return {"path": target, "turns": rollout.turns, "ended_at": rollout.now}


def write_index(codex_home, names, modified):
    """session_index.jsonl: one line per rename, oldest first, as codex
    appends them, last modified at `modified` (Unix seconds)."""
    os.makedirs(codex_home, exist_ok=True)
    index = os.path.join(codex_home, "session_index.jsonl")
    with open(index, "w") as file:
        for entry in sorted(names, key=lambda entry: entry["updated_at"]):
            file.write(json.dumps(entry, separators=(",", ":")) + "\n")
    os.utime(index, (modified, modified))


def lock_path(codex_home, thread_id):
    return os.path.join(codex_home, "thread-writer-locks", f"{thread_id}.lock")


def hold_writer_locks(codex_home, thread_ids):
    """Takes each thread's writer lock, as a live codex writing that thread
    does, and returns the open files: the locks last while they stay open."""
    held = []
    for thread_id in thread_ids:
        target = lock_path(codex_home, thread_id)
        os.makedirs(os.path.dirname(target), exist_ok=True)
        file = open(target, "a+")
        try:
            fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            file.close()
            raise SystemExit(f"The writer lock for thread {thread_id} is already held by another process.") from None
        held.append(file)
    return held
