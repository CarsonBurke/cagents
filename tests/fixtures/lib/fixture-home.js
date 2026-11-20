// Shared CommonJS guard for offline JavaScript providers. No writes on import.
const fs = require('node:fs');
const path = require('node:path');

const FIXTURE_ROOT_MARKER = '.cagents-fixture-root';

// Resolve existing symlinks even when the final config directory is absent.
function realpath(value, depth = 0) {
    if (depth > 40) throw new Error('too many symbolic links');
    const absolute = path.isAbsolute(value) ? value : `${process.cwd()}/${value}`;
    let resolved = path.parse(absolute).root;
    for (const component of absolute.split(path.sep)) {
        if (!component || component === '.') continue;
        if (component === '..') {
            resolved = path.dirname(resolved);
            continue;
        }
        resolved = path.join(resolved, component);
        try {
            if (fs.lstatSync(resolved).isSymbolicLink()) {
                const target = fs.readlinkSync(resolved);
                resolved = realpath(path.isAbsolute(target) ? target : `${path.dirname(resolved)}/${target}`, depth + 1);
            }
        } catch (error) {
            if (error.code !== 'ENOENT' && error.code !== 'ENOTDIR') throw error;
        }
    }
    return resolved;
}

function requireFixtureHome(variable, value = process.env[variable]) {
    let resolved = '<unset>';
    if (value) {
        try {
            resolved = realpath(value);
            for (let ancestor = resolved; ; ancestor = path.dirname(ancestor)) {
                try {
                    if (fs.statSync(path.join(ancestor, FIXTURE_ROOT_MARKER)).isFile()) return resolved;
                } catch (error) {
                    if (error.code !== 'ENOENT' && error.code !== 'ENOTDIR') throw error;
                }
                if (path.dirname(ancestor) === ancestor) break;
            }
        } catch (_) {
            resolved = path.resolve(value);
        }
    }
    const display = resolved.replace(/\r/g, '\\r').replace(/\n/g, '\\n');
    process.stderr.write(`fixture-home: refusing ${variable}=${display}: not inside a marked fixture home\n`);
    process.exit(1);
}

module.exports = { FIXTURE_ROOT_MARKER, requireFixtureHome };
