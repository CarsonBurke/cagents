"""Extract explicitly selected Rust declarations for a source contract.

This is a bounded lexical extractor, not a Rust parser or a serializer schema
generator. It does not resolve imports, expand macros, evaluate cfg/expressions,
or follow field types and custom serializers. Callers choose the covered items.
"""

from __future__ import annotations

import re


class SourceError(ValueError):
    """The selected source cannot be extracted without ambiguity."""


_OPEN = {"(": ")", "[": "]", "{": "}"}
_CLOSE = {value: key for key, value in _OPEN.items()}
_PUNCTUATION = set("()[]{}=<>!~+-*/%^&|@.,;:#$?")
_ORDINARY_DERIVES = {
    "Clone", "Copy", "Debug", "Default", "Eq", "Hash", "Ord", "PartialEq",
    "PartialOrd",
}
_IGNORED_ATTRIBUTES = {"allow", "deny", "doc", "expect", "forbid", "warn"}
_RAW_STRING = re.compile(r'(?:br|cr|r)(#*)"')


def _identifier_start(character: str) -> bool:
    return character == "_" or character.isidentifier()


def _identifier_continue(character: str) -> bool:
    return ("a" + character).isidentifier()


def _quoted_end(source: str, quote: int) -> int:
    cursor = quote + 1
    while cursor < len(source):
        if source[cursor] == "\\":
            cursor += 2
        elif source[cursor] == '"':
            return cursor + 1
        else:
            cursor += 1
    raise SourceError(f"unterminated string at offset {quote}")


def _character_end(source: str, quote: int) -> int | None:
    """Return a char literal's end, or None for a possible lifetime."""
    cursor = quote + 1
    if cursor >= len(source):
        return None
    if source[cursor] == "\\":
        cursor += 1
        if cursor >= len(source):
            return None
        if source[cursor] == "u" and source[cursor + 1:cursor + 2] == "{":
            closing = source.find("}", cursor + 2)
            if closing == -1:
                raise SourceError(f"unterminated character escape at offset {quote}")
            cursor = closing + 1
        elif source[cursor] == "x":
            cursor += 3
        else:
            cursor += 1
    else:
        if source[cursor] in "'\r\n":
            return None
        cursor += 1
    return cursor + 1 if source[cursor:cursor + 1] == "'" else None


def _tokens(source: str) -> list[str]:
    result: list[str] = []
    cursor = 0
    while cursor < len(source):
        character = source[cursor]
        if character.isspace() or (cursor == 0 and character == "\ufeff"):
            cursor += 1
            continue
        if source.startswith("//", cursor):
            newline = source.find("\n", cursor + 2)
            cursor = len(source) if newline == -1 else newline + 1
            continue
        if source.startswith("/*", cursor):
            start = cursor
            depth = 1
            cursor += 2
            while cursor < len(source) and depth:
                if source.startswith("/*", cursor):
                    depth += 1
                    cursor += 2
                elif source.startswith("*/", cursor):
                    depth -= 1
                    cursor += 2
                else:
                    cursor += 1
            if depth:
                raise SourceError(f"unterminated block comment at offset {start}")
            continue
        raw = _RAW_STRING.match(source, cursor)
        if raw:
            terminator = '"' + raw.group(1)
            closing = source.find(terminator, raw.end())
            if closing == -1:
                raise SourceError(f"unterminated raw string at offset {cursor}")
            end = closing + len(terminator)
            result.append(source[cursor:end])
            cursor = end
            continue
        if character == '"' or (
            character in "bc" and source[cursor + 1:cursor + 2] == '"'
        ):
            quote = cursor if character == '"' else cursor + 1
            end = _quoted_end(source, quote)
            result.append(source[cursor:end])
            cursor = end
            continue
        if character == "'" or (
            character == "b" and source[cursor + 1:cursor + 2] == "'"
        ):
            quote = cursor if character == "'" else cursor + 1
            end = _character_end(source, quote)
            if end is not None:
                result.append(source[cursor:end])
                cursor = end
                continue
            if character == "'" and source[cursor + 1:cursor + 2] and (
                _identifier_start(source[cursor + 1])
            ):
                end = cursor + 2
                while end < len(source) and _identifier_continue(source[end]):
                    end += 1
                result.append(source[cursor:end])
                cursor = end
                continue
            raise SourceError(f"unrecognized character literal at offset {cursor}")
        if _identifier_start(character):
            end = cursor + 1
            if source.startswith("r#", cursor):
                end = cursor + 2
                if end == len(source) or not _identifier_start(source[end]):
                    raise SourceError(f"invalid raw identifier at offset {cursor}")
                end += 1
            while end < len(source) and _identifier_continue(source[end]):
                end += 1
            result.append(source[cursor:end])
            cursor = end
            continue
        if character.isascii() and character.isdigit():
            end = cursor + 1
            while end < len(source) and (
                source[end].isascii() and (source[end].isalnum() or source[end] == "_")
            ):
                end += 1
            result.append(source[cursor:end])
            cursor = end
            continue
        if character in _PUNCTUATION:
            result.append(character)
            cursor += 1
            continue
        raise SourceError(f"unrecognized Rust token at offset {cursor}")
    return result


def _delimiters(tokens: list[str]) -> dict[int, int]:
    pairs: dict[int, int] = {}
    stack: list[int] = []
    for index, token in enumerate(tokens):
        if token in _OPEN:
            stack.append(index)
        elif token in _CLOSE:
            if not stack or tokens[stack[-1]] != _CLOSE[token]:
                raise SourceError(f"unmatched delimiter {token!r} at token {index}")
            opening = stack.pop()
            pairs[opening] = index
            pairs[index] = opening
    if stack:
        raise SourceError(f"unclosed delimiter at token {stack[-1]}")
    return pairs


def _generic_end(tokens: list[str], start: int, pairs: dict[int, int]) -> int:
    depth = 0
    cursor = start
    while cursor < len(tokens):
        token = tokens[cursor]
        if token in _OPEN:
            cursor = pairs[cursor] + 1
            continue
        if token == "<":
            depth += 1
        elif token == ">" and (cursor == 0 or tokens[cursor - 1] != "-"):
            depth -= 1
            if depth == 0:
                return cursor + 1
        elif token == ";":
            break
        cursor += 1
    raise SourceError(f"unterminated generic header at token {start}")


def _item_end(tokens: list[str], kind: int, pairs: dict[int, int]) -> int:
    cursor = kind + 2
    tuple_struct = False
    if cursor < len(tokens) and tokens[cursor] == "<":
        cursor = _generic_end(tokens, cursor, pairs)
    if cursor == len(tokens):
        raise SourceError("selected declaration has no body")
    if tokens[cursor] == "(" and tokens[kind] == "struct":
        tuple_struct = True
        cursor = pairs[cursor] + 1
        if cursor < len(tokens) and tokens[cursor] == ";":
            return cursor + 1
        if cursor == len(tokens) or tokens[cursor] != "where":
            raise SourceError("tuple struct must end with a semicolon")
    if tokens[cursor] == "where":
        cursor += 1
        while cursor < len(tokens) and tokens[cursor] not in {"{", ";"}:
            if tokens[cursor] in {"(", "["}:
                cursor = pairs[cursor] + 1
            elif tokens[cursor] == "<":
                cursor = _generic_end(tokens, cursor, pairs)
            else:
                cursor += 1
    if cursor < len(tokens) and tokens[cursor] == "{" and not tuple_struct:
        return pairs[cursor] + 1
    if cursor < len(tokens) and tokens[cursor] == ";" and tokens[kind] == "struct":
        return cursor + 1
    raise SourceError(f"unsupported selected declaration header at token {cursor}")


def _item_start(tokens: list[str], kind: int, pairs: dict[int, int]) -> int:
    start = kind
    if start and tokens[start - 1] == ")":
        opening = pairs[start - 1]
        if opening and tokens[opening - 1] == "pub":
            start = opening - 1
    elif start and tokens[start - 1] == "pub":
        start -= 1
    while start and tokens[start - 1] == "]":
        opening = pairs[start - 1]
        if not opening or tokens[opening - 1] != "#":
            break
        start = opening - 1
    return start


def _derive_arguments(tokens: list[str]) -> list[list[str]]:
    pairs = _delimiters(tokens)
    arguments: list[list[str]] = []
    start = cursor = 0
    while cursor < len(tokens):
        if tokens[cursor] in _OPEN:
            cursor = pairs[cursor] + 1
        elif tokens[cursor] == ",":
            if cursor == start:
                raise SourceError("empty derive argument")
            arguments.append(tokens[start:cursor])
            start = cursor = cursor + 1
        else:
            cursor += 1
    if start < len(tokens):
        arguments.append(tokens[start:])
    return arguments


def _normalise(tokens: list[str]) -> list[str]:
    pairs = _delimiters(tokens)
    result: list[str] = []
    cursor = 0
    while cursor < len(tokens):
        if tokens[cursor:cursor + 2] == ["#", "["]:
            end = pairs[cursor + 1]
            attribute = tokens[cursor + 2:end]
            if not attribute:
                raise SourceError("empty selected attribute")
            if attribute[0] in _IGNORED_ATTRIBUTES and attribute[1:2] != [":"]:
                cursor = end + 1
                continue
            if attribute[0] == "derive":
                if attribute[1:2] != ["("] or attribute[-1:] != [")"]:
                    raise SourceError("unsupported derive attribute")
                arguments = [
                    argument for argument in _derive_arguments(attribute[2:-1])
                    if len(argument) != 1 or argument[0] not in _ORDINARY_DERIVES
                ]
                if arguments:
                    result.extend(["#", "[", "derive", "("])
                    for index, argument in enumerate(arguments):
                        if index:
                            result.append(",")
                        result.extend(argument)
                    result.extend([")", "]"])
                cursor = end + 1
                continue
        result.append(tokens[cursor])
        cursor += 1
    return result


def selected_items(source: str, names: tuple[str, ...]) -> dict[str, list[str]]:
    """Return normalized tokens for unique, explicitly selected declarations.

    Selection is by declaration name in this source, not by module path or type
    reachability. A missing/duplicate name or unsupported selected header fails
    closed. Macro-generated declarations are not expanded.
    """
    if len(set(names)) != len(names):
        raise SourceError("duplicate requested declaration name")
    if any(not name.isidentifier() for name in names):
        raise SourceError("invalid requested declaration name")
    tokens = _tokens(source)
    pairs = _delimiters(tokens)
    selected = set(names)
    result: dict[str, list[str]] = {}
    for index, token in enumerate(tokens[:-1]):
        if token not in {"struct", "enum"}:
            continue
        name = tokens[index + 1].removeprefix("r#")
        if name not in selected:
            continue
        if name in result:
            raise SourceError(f"duplicate selected declaration: {name}")
        start = _item_start(tokens, index, pairs)
        end = _item_end(tokens, index, pairs)
        result[name] = _normalise(tokens[start:end])
    missing = selected - result.keys()
    if missing:
        raise SourceError(f"missing selected declarations: {', '.join(sorted(missing))}")
    return {name: result[name] for name in names}


def protocol_version(source: str) -> int:
    """Read one literal u16 PROTOCOL_VERSION; expressions fail closed."""
    tokens = _tokens(source)
    _delimiters(tokens)
    positions = [
        index for index in range(len(tokens) - 1)
        if tokens[index:index + 2] == ["const", "PROTOCOL_VERSION"]
    ]
    if len(positions) != 1:
        raise SourceError("expected exactly one PROTOCOL_VERSION declaration")
    start = positions[0] + 2
    if tokens[start:start + 3] != [":", "u16", "="] or (
        tokens[start + 4:start + 5] != [";"]
    ):
        raise SourceError("PROTOCOL_VERSION must be a literal u16")
    literal = tokens[start + 3]
    match = re.fullmatch(
        r"(0x[0-9a-fA-F_]+|0o[0-7_]+|0b[01_]+|[0-9][0-9_]*)(?:u16)?",
        literal,
    )
    if not match:
        raise SourceError("PROTOCOL_VERSION must be a literal u16")
    digits = match.group(1).replace("_", "")
    value = int(digits, 0 if digits.startswith(("0x", "0o", "0b")) else 10)
    if not 0 <= value <= 65535:
        raise SourceError("PROTOCOL_VERSION is outside u16")
    return value
