#!/usr/bin/env python3
"""Encode frozen c07 protocol-127 data; no Rust or product serializer is used.

The declarations and ordinals here were transcribed from c07b6f78e906871b0e8ef98e318b27b0b86c268b.
This is a fixture encoder, not a protocol client, provider, or running legacy service.
"""

from __future__ import annotations

import argparse
import hashlib
from pathlib import Path
import uuid


PROTOCOL = 127
SAVED_ID = "00000000-0000-4000-8000-000000000127"
RETAINED_ID = "00000000-0000-4000-8000-000000000128"
SAVED_CONVERSATION = "11111111-1111-4111-8111-111111111111"
RETAINED_CONVERSATION = "22222222-2222-4222-8222-222222222222"
CURRENT_PROOF = ("source", "boot", "actor")
SAVED_PROOF = ("saved", "boot", "previous")
READ_ONLY_REASON = "Saved conversation history is read-only; no provider was started."
ITEM_ID = "frozen127-history"
ITEM_TEXT = "frozen127 retained history"
ERROR_TEXT = "legacy source refused"


def unsigned(value: int) -> bytes:
    """Postcard's unsigned LEB128, used for integer values and variant tags."""
    assert 0 <= value <= (1 << 64) - 1
    encoded = bytearray()
    while value >= 128:
        encoded.append((value & 127) | 128)
        value >>= 7
    encoded.append(value)
    return bytes(encoded)


def boolean(value: bool) -> bytes:
    return b"\x01" if value else b"\x00"


def text(value: str) -> bytes:
    encoded = value.encode("utf-8")
    return unsigned(len(encoded)) + encoded


def option(encoded: bytes | None) -> bytes:
    return b"\x00" if encoded is None else b"\x01" + encoded


def sequence(values: list[bytes]) -> bytes:
    return unsigned(len(values)) + b"".join(values)


def identifier(value: str) -> bytes:
    # Frozen model::TerminalId is serde-transparent over uuid::Uuid. UUID's
    # non-human serializer calls serialize_bytes, not [u8;16] serialization.
    encoded = uuid.UUID(value).bytes
    return unsigned(len(encoded)) + encoded


def proof(value: tuple[str, str, str]) -> bytes:
    # model::HostSessionProof: host_session_id, boot_id, host_incarnation.
    return b"".join(text(component) for component in value)


def recipe() -> bytes:
    # model::LaunchRecipe: program, arguments, working_directory,
    # environment (BTreeMap), unset_environment (BTreeSet).
    return b"".join(
        [
            text("claude"),
            sequence([text("--model"), text("fixture")]),
            text("/fixture/removed-worktree"),
            unsigned(1),
            text("CLAUDE_CONFIG_DIR"),
            text("/fixture/claude-config"),
            sequence([text("FIXTURE_UNSET")]),
        ]
    )


def session(*, retained: bool = False, requested: bool = False) -> bytes:
    # chat::Session: id, launch, proof, terminal, owned_codex_listener,
    # native_continuity. Launch: provider, recipe, session_id.
    # Provider::Claude=1; NativeContinuity::ReconnectOnly=0,
    # NativeContinuity::RetainedClaudeActor=2 with protocol:u32=2.
    source_proof = CURRENT_PROOF if retained or requested else SAVED_PROOF
    return b"".join(
        [
            identifier(RETAINED_ID if retained else SAVED_ID),
            unsigned(1),
            recipe(),
            option(text(RETAINED_CONVERSATION if retained else SAVED_CONVERSATION)),
            option(proof(source_proof)),
            option(None),  # terminal
            option(None),  # owned_codex_listener
            unsigned(2) + unsigned(2) if retained else unsigned(0),
        ]
    )


def capabilities() -> bytes:
    # Frozen chat::Capabilities has fifteen serialized fields in this order.
    fields = [
        boolean(False),  # steer
        boolean(False),  # queue
        boolean(False),  # attachments
        boolean(False),  # compact
        boolean(False),  # model_selection
        boolean(False),  # reasoning_selection
        boolean(False),  # fast_mode
        sequence([]),  # permission_modes
        unsigned(0),  # skill_reach: SkillReach::Anywhere=0
        boolean(False),  # queue_remove
        boolean(False),  # queue_start
        sequence([]),  # interrupt_modes
        boolean(False),  # branch
        boolean(False),  # stop_subagent
        boolean(False),  # interrupt_stops_background
    ]
    assert len(fields) == 15
    return b"".join(fields)


def history_item() -> bytes:
    # Item: started_at, id, role, content, at. Arc<Item> serializes its
    # pointee. Role::Assistant=1, ItemContent::Text=0, Phase::Final=0.
    return b"".join(
        [
            option(None),
            text(ITEM_ID),
            unsigned(1),
            unsigned(0),
            text(ITEM_TEXT),
            boolean(False),
            unsigned(0),
            option(None),
        ]
    )


def file_changes() -> bytes:
    # FileChangesSummary(request_id, revision, files, total_files,
    # applied_files, has_more, coverage). Arc adds no bytes. Coverage has
    # four booleans: unloaded, imported, incomplete delegated, uncached.
    return b"".join(
        [
            unsigned(0),
            unsigned(0),
            sequence([]),
            unsigned(0),
            unsigned(0),
            boolean(False),
            boolean(False),
            boolean(False),
            boolean(False),
            boolean(False),
        ]
    )


def snapshot(*, loading: bool) -> bytes:
    # Frozen chat::Snapshot declaration order, not Rust literal order. Its
    # notification_preview field is #[serde(skip)] and contributes no bytes.
    # Every #[serde(default)] field below still occupies its wire position.
    fields = [
        ("sampled_at", option(None)),
        ("generation", unsigned(0)),
        ("turn_generation", unsigned(0)),
        ("finished_at", option(None)),
        ("items", sequence([] if loading else [history_item()])),
        ("text_copy_sources", unsigned(0) + sequence([]) + unsigned(0)),
        ("requests", sequence([])),
        ("capabilities", capabilities()),
        ("models", sequence([])),
        ("selected_model", option(None)),
        ("reasoning", option(None)),
        ("fast_mode", option(None)),
        ("permission_mode", option(None)),
        ("permissions", option(None)),
        ("permission_denials", sequence([])),
        ("commands", sequence([])),
        ("skills", sequence([])),
        ("usage", unsigned(0) * 4 + option(None) + option(None)),
        ("busy", boolean(False)),
        ("detail", text("")),
        ("session_id", option(text(SAVED_CONVERSATION))),
        ("native_continuity", unsigned(0)),
        ("title", option(None)),
        ("error", option(None)),
        ("read_only_reason", option(text(READ_ONLY_REASON))),
        ("exited", boolean(True)),
        ("has_earlier", boolean(False)),
        ("has_later", boolean(False)),
        ("composer", option(None)),
        ("prompt_suggestion", option(None)),
        ("open_url", option(None)),
        ("waiting", boolean(False)),
        ("plan", sequence([])),
        ("subagents", sequence([])),
        ("ready", boolean(not loading)),
        ("queued_messages", sequence([])),
        ("queue_removals", sequence([])),
        ("history_loading", boolean(loading)),
        ("interrupt_mode", option(None)),
        ("branch_points", sequence([])),
        ("forked", option(None)),
        ("file_changes", file_changes()),
    ]
    assert len(fields) == 42
    return b"".join(encoded for _, encoded in fields)


def framed(payload: bytes) -> bytes:
    # The transport prefix is a fixed big-endian u32, not a postcard integer.
    return len(payload).to_bytes(4, "big") + payload


def request(request_id: int, tag: int, body: bytes = b"") -> bytes:
    # ClientEnvelope(version, request_id, proof:Option, request).
    return framed(
        unsigned(PROTOCOL)
        + unsigned(request_id)
        + option(None if request_id == 1 else proof(CURRENT_PROOF))
        + unsigned(tag)
        + body
    )


def response(request_id: int, tag: int, body: bytes = b"") -> bytes:
    # ServerEnvelope(version, request_id:Some, proof, message:Response=0).
    return framed(
        unsigned(PROTOCOL)
        + option(unsigned(request_id))
        + proof(CURRENT_PROOF)
        + unsigned(0)
        + unsigned(tag)
        + body
    )


def error(source_proof: tuple[str, str, str]) -> bytes:
    # Frozen ServerMessage::Error=6 and ProtocolError has ONLY these three
    # fields: code PermissionDenied=10, message, retryable=false. No details.
    return framed(
        unsigned(PROTOCOL)
        + option(unsigned(5))
        + proof(source_proof)
        + unsigned(6)
        + unsigned(10)
        + text(ERROR_TEXT)
        + boolean(False)
    )


def fixtures() -> dict[str, bytes]:
    result = {
        "request-01-connect.bin": request(1, 0, option(proof(CURRENT_PROOF)) + sequence([])),
        "response-01-connected.bin": response(1, 0, proof(CURRENT_PROOF) + sequence([])),
        "request-02-chat-list.bin": request(2, 29),
        "response-02-chat-listed.bin": response(2, 13, sequence([session(), session(retained=True)])),
        "request-03-list.bin": request(3, 1),
        "response-03-listed.bin": response(3, 1, sequence([])),
        "request-04-resolve.bin": request(4, 38, session(retained=True)),
        "response-04-resolved.bin": response(4, 18, session(retained=True)),
        "request-05-chat-attach.bin": request(5, 22, session(requested=True)),
        "response-05-chat-attached.bin": response(5, 11, session(requested=True) + snapshot(loading=True)),
        "request-06-chat-visibility.bin": request(6, 25, identifier(SAVED_ID) + boolean(True)),
        "response-06-accepted.bin": response(6, 5),
        "request-07-chat-read.bin": request(7, 26, identifier(SAVED_ID) + option(None)),
        "response-07-chat-snapshot.bin": response(7, 12, identifier(SAVED_ID) + option(snapshot(loading=False))),
        "request-08-chat-detach.bin": request(8, 23, identifier(SAVED_ID)),
        "response-08-detached.bin": response(8, 4),
        "event-chat-changed.bin": framed(unsigned(PROTOCOL) + option(None) + proof(CURRENT_PROOF) + unsigned(7) + identifier(SAVED_ID)),
        "error-permission-denied.bin": error(CURRENT_PROOF),
        "error-minimal.bin": error(("s", "b", "i")),
    }
    # Hand-authored independent literal cross-check: length34, version127,
    # requestSome(5), proof(s,b,i), Error6, PermissionDenied10, string21, false.
    minimal_error = bytes.fromhex(
        "00000022 7f 01 05 0173 0162 0169 06 0a 15 "
        "6c656761637920736f757263652072656675736564 00"
    )
    assert result["error-minimal.bin"] == minimal_error
    for data in result.values():
        assert int.from_bytes(data[:4], "big") == len(data) - 4
        assert data[4] == 127
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="compare without writing fixture files")
    args = parser.parse_args()
    directory = Path(__file__).resolve().parent
    expected = fixtures()
    for name, data in expected.items():
        target = directory / name
        if args.check:
            if not target.is_file() or target.read_bytes() != data:
                raise SystemExit(f"frozen protocol-127 fixture differs: {name}")
        else:
            target.write_bytes(data)
        print(f"{name} {len(data)} {hashlib.sha256(data).hexdigest()}")
    unexpected = {path.name for path in directory.glob("*.bin")} - expected.keys()
    if unexpected:
        raise SystemExit(f"unrecognized fixture files: {sorted(unexpected)}")


if __name__ == "__main__":
    main()
