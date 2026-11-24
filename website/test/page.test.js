import assert from "node:assert/strict";
import { existsSync, readFileSync, statSync } from "node:fs";
import test from "node:test";
import { loadScreens, renderPage } from "../lib/screens.js";

const root = new URL("../", import.meta.url);
const repository = new URL("../", root);
const screens = loadScreens(root);
const page = renderPage(readFileSync(new URL("index.html", root), "utf8"), {
  screens,
  installCommand: readFileSync(new URL("src/install-command.sh", root), "utf8"),
});

test("every screenshot has both formats in every theme it lists, at a sensible weight", () => {
  for (const [name, screen] of Object.entries(screens)) {
    assert.ok(screen.alt.length > 20, `${name} needs alt text that describes it`);
    for (const file of Object.values(screen.files)) {
      for (const format of ["avif", "webp"]) {
        const path = new URL(`${file}.${format}`, root);
        assert.ok(existsSync(path), `${file}.${format} is missing`);
        assert.ok(statSync(path).size < 400_000, `${file}.${format} is over 400 kB`);
      }
    }
  }
});

test("images are sized, described and lazy below the fold", () => {
  const images = [...page.matchAll(/<img\b[^>]*>/g)].map(([tag]) => tag);
  assert.ok(images.length > 4);
  for (const tag of images) {
    assert.match(tag, /\balt="/, tag);
    if (tag.includes("/screens/")) {
      assert.match(tag, /\bwidth="\d+" height="\d+"/, tag);
    }
  }
  const shots = images.filter((tag) => tag.includes("/screens/"));
  assert.match(shots[0], /fetchpriority="high"/, "the hero loads first");
  for (const tag of shots.slice(1)) assert.match(tag, /loading="lazy"/, tag);
});

test("in-page links and repository links lead somewhere", () => {
  const ids = new Set([...page.matchAll(/\bid="([^"]+)"/g)].map(([, id]) => id));
  for (const [, target] of page.matchAll(/href="#([^"]*)"/g)) {
    assert.ok(ids.has(target), `#${target} has no target`);
  }
  for (const [, path] of page.matchAll(/https:\/\/github\.com\/CarsonBurke\/cagents\/blob\/main\/([^"#]+)/g)) {
    assert.ok(existsSync(new URL(path, repository)), `${path} is not in the repository`);
  }
});

test("headings descend without skipping a level", () => {
  const levels = [...page.matchAll(/<h([1-6])\b/g)].map(([, level]) => Number(level));
  assert.equal(levels[0], 1);
  assert.equal(levels.filter((level) => level === 1).length, 1);
  levels.reduce((previous, level) => {
    assert.ok(level <= previous + 1, `h${level} follows h${previous}`);
    return level;
  });
});

test("the README's screenshots are in the repository", () => {
  const readme = readFileSync(new URL("README.md", repository), "utf8");
  const paths = [
    ...readme.matchAll(/(?:src|srcset)="(website\/[^"]+)"/g),
    ...readme.matchAll(/\]\((website\/[^)]+)\)/g),
  ].map(([, path]) => path);
  assert.ok(paths.length >= 5, "the README shows the hero and its tour");
  for (const path of paths) assert.ok(existsSync(new URL(path, repository)), `${path} is missing`);
});
