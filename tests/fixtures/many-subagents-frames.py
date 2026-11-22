#!/usr/bin/env python3
"""Write fixture-frames.jsonl for a Claude turn that starts many background
agents in the shape Claude Code 2.1.325 reports them, for the subagent tabs
and chooser (see "fixture-frames.jsonl" in docs/chat-composer-fixture.md).

    many-subagents-frames.py FRAMES [--finished N]

Three agents keep working, each on a running command of its own; one fails;
the rest finish with a one-line report. The finished ones come back a moment
apart, so the chooser has times to order them by. The turn then answers and
ends while the working agents go on.
"""
import argparse
import json

WORKING = [
    ("Profile the render loop", "cargo bench --bench frame_times"),
    ("Trace the retry path", "rg -n 'retry_budget' src"),
    ("Audit the session store", "sqlite3 sessions.db '.schema'"),
]
FAILED = ("Run the soak test", "The soak test crashed after 14 minutes: out of memory.")
FINISHED = [
    ("Review the cache key change", "No problem found; the key now includes the locale."),
    ("Summarize the open issues", "12 open issues, 3 of them regressions from last week."),
    ("Check the release notes", "Two entries were missing; added them."),
    ("Find unused settings", "Four settings are never read; listed them with their callers."),
    ("Measure startup time", "Startup takes 410 ms cold and 120 ms warm."),
    ("Explain the flaky test", "The test races the file watcher; a fix is sketched."),
    ("Compare the two parsers", "The new parser is 2.3 times faster on large files."),
    ("Draft the migration guide", "Drafted the guide in docs/migrate.md."),
    ("List the deprecated APIs", "Nine deprecated APIs remain, all internal."),
    ("Verify the license headers", "Every source file carries a header."),
    ("Check the error messages", "Three messages named internal types; reworded them."),
    ("Map the plugin hooks", "Seven hooks, of which two are undocumented."),
]


def frame(**fields):
    return json.dumps(fields, separators=(",", ":"))


def launch(call, description, prompt):
    return [
        frame(type="assistant", uuid=f"launch-{call}", message={
            "id": f"launch-{call}", "role": "assistant",
            "content": [{"type": "tool_use", "id": call, "name": "Agent", "input": {
                "description": description, "subagent_type": "general-purpose",
                "prompt": prompt, "run_in_background": True}}]}),
        frame(type="system", subtype="task_started", uuid=f"start-{call}",
              task_id=f"task-{call}", tool_use_id=call, task_type="local_agent",
              description=description, is_backgrounded=True, spawn_depth=1),
        frame(type="user", uuid=f"launched-{call}",
              tool_use_result={"status": "async_launched", "agentId": f"task-{call}"},
              message={"role": "user", "content": [{
                  "type": "tool_result", "tool_use_id": call,
                  "content": "Async agent launched"}]}),
    ]


def notice(call, description, status, result):
    verb = "finished" if status == "completed" else "failed"
    text = (
        f"<task-notification>\n<task-id>task-{call}</task-id>\n"
        f"<tool-use-id>{call}</tool-use-id>\n<output-file>/tmp/task-{call}.output</output-file>\n"
        f"<status>{status}</status>\n<summary>Agent \"{description}\" {verb}</summary>\n"
        f"<result>{result}</result>\n</task-notification>"
    )
    return frame(type="user", uuid=f"notice-{call}", origin={"kind": "task-notification"},
                 message={"role": "user", "content": text})


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("frames")
    parser.add_argument("--finished", type=int, default=len(FINISHED))
    arguments = parser.parse_args()
    lines = []
    finished = FINISHED[: arguments.finished]
    for index, (description, _) in enumerate(finished):
        lines += launch(f"toolu_done_{index}@TURN@", description, description + ".")
    for index, (description, command) in enumerate(WORKING):
        call = f"toolu_work_{index}@TURN@"
        lines += launch(call, description, description + ".")
        lines.append(frame(
            type="assistant", uuid=f"step-{call}", parent_tool_use_id=call,
            message={"id": f"step-{call}", "role": "assistant", "content": [{
                "type": "tool_use", "id": f"bash-{call}", "name": "Bash",
                "input": {"command": command, "description": description}}]}))
    failed = "toolu_failed@TURN@"
    lines += launch(failed, FAILED[0], FAILED[0] + ".")
    for index, (description, result) in enumerate(finished):
        lines.append(frame(_sleep=1))
        lines.append(notice(f"toolu_done_{index}@TURN@", description, "completed", result))
    lines.append(notice(failed, FAILED[0], "failed", FAILED[1]))
    with open(arguments.frames, "w") as output:
        output.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
