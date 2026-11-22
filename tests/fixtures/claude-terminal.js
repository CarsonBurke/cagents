#!/usr/bin/env node
// Offline interactive Claude Code for terminal panes.
//
// The synthetic provider (chat-composer-claude.sh) speaks stream-json for
// native chat. A terminal pane instead runs Claude Code's interactive CLI, and
// that script hands such a launch to this file under node, as an npm
// installation runs `node .../claude`, so cagents recognizes and observes it
// as Claude. It never contacts a model.
//
// Like Claude Code, it publishes the conversation it holds in
// CONFIG/sessions/PID.json, keyed by its start time, and keeps each
// conversation's transcript in CONFIG/projects/PROJECT/ID.jsonl:
//
//   claude --resume ID  resumes that saved conversation, or exits 1 if none
//   claude --resume     shows the saved conversations to pick from
//   /clear              switches to a new conversation, which has no
//                       transcript until its first message
//   /exit, Ctrl+D       exits and removes the process record
//
// Any other line is a prompt: it and an invented reply join the current
// conversation's transcript.
'use strict';

const crypto = require('crypto');
const fs = require('fs');
const path = require('path');
const readline = require('readline');

const os = require('os');

const config = process.env.CLAUDE_CONFIG_DIR || path.join(process.env.HOME, '.claude');
// Never write into the account's real Claude home, whatever HOME says.
const realpath = (file) => {
    try {
        return fs.realpathSync(file);
    } catch {
        return path.resolve(file);
    }
};
if (realpath(config) === realpath(path.join(os.userInfo().homedir, '.claude'))) {
    process.stderr.write("claude-terminal fixture: refusing the account's real Claude home\n");
    process.exit(1);
}
const cwd = fs.realpathSync(process.cwd());
const projectDirectory = path.join(config, 'projects', cwd.replace(/[^a-zA-Z0-9]/g, '-'));
const recordPath = path.join(config, 'sessions', `${process.pid}.json`);

let resumed = null;
let picker = false;
let session = null;
const argv = process.argv.slice(2);
for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === '--resume' || argument === '-r') {
        // As in Claude Code, the conversation is optional.
        const next = argv[index + 1];
        if (next !== undefined && !next.startsWith('-')) {
            resumed = next;
            index += 1;
        } else {
            picker = true;
        }
    } else if (argument.startsWith('--resume=')) {
        resumed = argument.slice('--resume='.length);
    } else if (argument === '--session-id') {
        session = argv[index + 1];
        index += 1;
    } else if (argument.startsWith('--session-id=')) {
        session = argument.slice('--session-id='.length);
    }
}

const out = (text) => process.stdout.write(text.replace(/\n/g, '\r\n'));
const dim = (text) => `\x1b[2m${text}\x1b[22m`;
const bold = (text) => `\x1b[1m${text}\x1b[22m`;
const accent = (text) => `\x1b[38;5;173m${text}\x1b[39m`;
const columns = () => Math.max(40, process.stdout.columns || 80);
const clip = (text, width) => (text.length > width ? `${text.slice(0, width - 1)}…` : text);

function transcriptPath(id) {
    return path.join(projectDirectory, `${id}.jsonl`);
}

function readTranscript(file) {
    let lines;
    try {
        lines = fs.readFileSync(file, 'utf8').split('\n');
    } catch {
        return null;
    }
    const messages = [];
    for (const line of lines) {
        let record;
        try {
            record = JSON.parse(line);
        } catch {
            continue;
        }
        if ((record.type !== 'user' && record.type !== 'assistant') || !record.message) {
            continue;
        }
        const content = record.message.content;
        const text = typeof content === 'string'
            ? content
            : Array.isArray(content)
                ? content.filter((part) => part.type === 'text').map((part) => part.text).join(' ')
                : '';
        if (text) {
            messages.push({ role: record.type, text: text.replace(/\s+/g, ' ').trim(), record });
        }
    }
    return messages;
}

// The kernel's start time, in clock ticks after boot, tells this process's
// record from one a later process with the same PID writes.
function processStart() {
    const stat = fs.readFileSync('/proc/self/stat', 'utf8');
    return stat.slice(stat.lastIndexOf(')') + 2).split(' ')[19];
}

function publish() {
    fs.mkdirSync(path.dirname(recordPath), { recursive: true, mode: 0o700 });
    const temporary = `${recordPath}.${crypto.randomUUID()}.tmp`;
    fs.writeFileSync(temporary, `${JSON.stringify({
        pid: process.pid,
        sessionId: session,
        cwd,
        startedAt: started,
        procStart: processStart(),
        kind: 'interactive',
        entrypoint: 'cli',
    })}\n`, { mode: 0o600 });
    fs.renameSync(temporary, recordPath);
}

function withdraw() {
    try {
        const record = JSON.parse(fs.readFileSync(recordPath, 'utf8'));
        if (record.pid === process.pid && record.procStart === processStart()) {
            fs.unlinkSync(recordPath);
        }
    } catch {
        // Nothing of ours to remove.
    }
}

function exit(status) {
    withdraw();
    if (process.stdin.isTTY) {
        process.stdin.setRawMode(false);
    }
    out('\n');
    process.exit(status);
}

function banner() {
    out(`${accent('✻')} ${bold('Claude Code')} ${dim('offline fixture')}\n`);
    out(`${dim(`  ${cwd}`)}\n\n`);
}

function describe() {
    const messages = readTranscript(transcriptPath(session));
    if (messages === null) {
        out(`${dim('Conversation')} ${session}\n`);
        out(`${dim('No messages yet; its transcript is written with the first one.')}\n\n`);
        return;
    }
    out(`${dim('Resumed conversation')} ${session}\n\n`);
    const width = columns() - 4;
    for (const message of messages.slice(-4)) {
        const mark = message.role === 'user' ? '>' : accent('●');
        out(`${mark} ${clip(message.text, width)}\n\n`);
    }
}

let line = '';
function prompt() {
    out(`${dim('─'.repeat(columns()))}\n> `);
}

function append(record) {
    fs.mkdirSync(projectDirectory, { recursive: true, mode: 0o700 });
    const previous = readTranscript(transcriptPath(session));
    const parent = previous && previous.length ? previous[previous.length - 1].record.uuid : null;
    const full = {
        ...record,
        uuid: crypto.randomUUID(),
        parentUuid: parent,
        sessionId: session,
        cwd,
        timestamp: new Date().toISOString(),
    };
    fs.appendFileSync(transcriptPath(session), `${JSON.stringify(full)}\n`, { mode: 0o600 });
}

function submit(text) {
    if (text === '/exit' || text === '/quit') {
        exit(0);
    }
    if (text === '/clear') {
        session = crypto.randomUUID();
        publish();
        out('\x1b[2J\x1b[H');
        banner();
        out(`${dim('Cleared. New conversation')} ${session}\n`);
        out(`${dim('No messages yet; its transcript is written with the first one.')}\n\n`);
        prompt();
        return;
    }
    if (text) {
        const reply = `Fixture reply: ${text}`;
        append({ type: 'user', message: { role: 'user', content: text } });
        append({
            type: 'assistant',
            message: { role: 'assistant', model: 'fixture', content: [{ type: 'text', text: reply }] },
        });
        out(`\n${accent('●')} ${reply}\n\n`);
    } else {
        out('\n');
    }
    prompt();
}

function conversations() {
    let entries;
    try {
        entries = fs.readdirSync(projectDirectory);
    } catch {
        return [];
    }
    return entries
        .filter((name) => /^[0-9a-f-]{36}\.jsonl$/.test(name))
        .map((name) => {
            const file = path.join(projectDirectory, name);
            const messages = readTranscript(file) || [];
            const first = messages.find((message) => message.role === 'user');
            return {
                id: name.slice(0, 36),
                title: first ? first.text : '(no prompt)',
                count: messages.length,
                modified: fs.statSync(file).mtimeMs,
            };
        })
        .filter((entry) => entry.count > 0)
        .sort((left, right) => right.modified - left.modified);
}

let choices = [];
let selected = 0;
function drawPicker() {
    out('\x1b[2J\x1b[H');
    banner();
    out(`${bold('Resume a conversation')}\n\n`);
    if (choices.length === 0) {
        out(`${dim('No saved conversations in this folder.')}\n`);
    }
    const width = columns() - 6;
    choices.forEach((choice, index) => {
        const pointer = index === selected ? accent('❯') : ' ';
        const title = index === selected ? bold(clip(choice.title, width)) : clip(choice.title, width);
        out(`${pointer} ${title}\n`);
        out(`  ${dim(`${choice.count} messages · ${choice.id}`)}\n\n`);
    });
    out(dim('↑/↓ to select · Enter to resume · Esc to start a new conversation'));
}

function choose(id) {
    picker = false;
    out('\x1b[2J\x1b[H');
    session = id || crypto.randomUUID();
    publish();
    banner();
    describe();
    prompt();
}

const started = Date.now();
if (!process.stdin.isTTY || !process.stdout.isTTY) {
    process.stderr.write('claude-terminal fixture: needs a terminal\n');
    process.exit(2);
}
if (resumed !== null) {
    if (readTranscript(transcriptPath(resumed)) === null) {
        process.stderr.write(`No conversation found with session ID: ${resumed}\n`);
        process.exit(1);
    }
    session = resumed;
} else if (session === null) {
    session = crypto.randomUUID();
}
for (const signal of ['SIGHUP', 'SIGTERM']) {
    process.on(signal, () => exit(129));
}
readline.emitKeypressEvents(process.stdin);
process.stdin.setRawMode(true);
process.stdin.on('keypress', (sequence, key = {}) => {
    if (key.ctrl && (key.name === 'c' || key.name === 'd')) {
        exit(0);
    }
    if (picker) {
        if (key.name === 'up') {
            selected = Math.max(0, selected - 1);
            drawPicker();
        } else if (key.name === 'down') {
            selected = Math.min(choices.length - 1, selected + 1);
            drawPicker();
        } else if (key.name === 'return') {
            choose(choices[selected] && choices[selected].id);
        } else if (key.name === 'escape') {
            choose(null);
        }
        return;
    }
    if (key.name === 'return' || key.name === 'enter') {
        const text = line.trim();
        line = '';
        submit(text);
    } else if (key.name === 'backspace') {
        if (line) {
            line = line.slice(0, -1);
            out('\b \b');
        }
    } else if (sequence && sequence.length === 1 && sequence >= ' ') {
        line += sequence;
        out(sequence);
    }
});
process.stdout.on('resize', () => {
    if (picker) {
        drawPicker();
    }
});

if (picker) {
    // The picker holds no conversation, so it publishes no record until one
    // is chosen, as Claude Code's does.
    choices = conversations();
    drawPicker();
} else {
    publish();
    banner();
    describe();
    prompt();
}
