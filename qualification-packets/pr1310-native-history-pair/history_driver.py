"""Finite PR1310 native History methods; original-root interface UNSUPPLIED.

No standalone runtime is admitted. Native imports are deliberately not here.
The future original owner must supply the binding in root-interface.md, then
obtain independent review of the assembled executable BEFORE use.
"""

import re

from observations import (ADAPTERS, EMPTY, FIRST_PAGER, Hold, LIMIT, LOADING,
                          PAGER, SEARCH, UNBORN, outcome, require, row_text)


class HistoryDriver:
    """Interactions only: no process launch, signals, admission or teardown.

    root is an UNIMPLEMENTED original-owner interface, not a JSON grant parser.
    atspi and image are actual pyatspi and PIL.Image modules supplied by that
    owner only after private endpoint and original-root admission. No mocks are
    supplied. A root checkpoint must fail before every effect on any unknown.
    """

    def __init__(self, root, atspi, image, arm, fixture):
        require(arm in ADAPTERS, "unknown native arm")
        self.root, self.ax, self.Image = root, atspi, image
        self.arm, self.fixture = arm, fixture
        self.binding = root.require_history_root(arm, *ADAPTERS[arm], fixture)
        # Original owner—not these descriptive integers—must prove authority.
        self.pid, self.xid = self.binding.ui_pid, self.binding.xid
        self.size = None
        self.sequence = 0
        self.row_records = fixture.records + ((fixture.sibling_record(),) if fixture.sibling else ())
        self.subjects = {row_text(record)[0]: record for record in self.row_records}
        require(len(self.subjects) == len(self.row_records), "ambiguous fixture subjects")
        self.check("attach")

    def check(self, operation):
        # Must be a remaining absolute budget, not a freshly reset timeout.
        remaining = self.root.check_history_root(self.binding, operation)
        require(remaining > 0, "original root budget exhausted")
        return remaining

    def record(self, event, **fields):
        self.check("evidence-before-write")
        self.sequence += 1
        self.root.append_history_observation(self.binding, {
            "sequence": self.sequence, "arm": self.arm, "event": event,
            "source": {"head": ADAPTERS[self.arm][0], "tree": ADAPTERS[self.arm][1]},
            "fixture_manifest_sha256": self.fixture.manifest_sha256,
            **fields,
        })
        self.check("evidence-after-write")

    def tool(self, argv):
        self.check("tool-before")
        # Retained original command handles/output/waits belong to root.
        result = self.root.run_history_tool(self.binding, tuple(argv), output_limit=262144)
        self.check("tool-after")
        require(result.exit_code == 0, "native tool failed; retain raw result")
        return result.stdout.decode("utf-8", errors="strict").strip()

    def poll(self, predicate, operation):
        # Only read-only observations may be retried, never input commands.
        while True:
            self.check(operation + "-before-read")
            value = predicate()
            self.check(operation + "-after-read")
            if value:
                return value
            self.root.pause_history_poll(self.binding, maximum_seconds=0.05)
            self.check(operation + "-after-pause")

    def nodes(self):
        self.check("AX-before")
        desktop = self.ax.Registry.getDesktop(0)
        require(desktop is not None, "private accessibility desktop absent")
        require(desktop.childCount < 1000, "accessibility desktop exceeds bound")
        apps = [desktop.getChildAtIndex(i) for i in range(desktop.childCount)]
        require(all(app is not None for app in apps), "changing accessibility desktop")
        apps = [app for app in apps if app.get_process_id() == self.pid]
        require(len(apps) == 1, "owned UI accessibility application absent/ambiguous")
        pending, result = [(apps[0], ())], []
        while pending:
            self.check("AX-node")
            node, path = pending.pop()
            require(node is not None and len(result) < 10000 and node.childCount < 1000,
                    "incomplete/oversized accessibility tree")
            result.append((path, node))
            pending.extend((node.getChildAtIndex(i), path + (i,))
                           for i in reversed(range(node.childCount)))
        self.check("AX-after")
        return result

    def showing(self, node):
        return node.getState().contains(self.ax.STATE_SHOWING)

    def enabled(self, node):
        return node.getState().contains(self.ax.STATE_ENABLED)

    def description(self, path, node):
        self.check("AX-description-before")
        states = node.getState()
        bounds = node.queryComponent().getExtents(self.ax.DESKTOP_COORDS)
        actions = []
        if node.getRole() == self.ax.ROLE_PUSH_BUTTON:
            action = node.queryAction()
            actions = [action.getName(i) for i in range(action.nActions)]
        value = {"path": list(path), "role": node.getRoleName(), "name": node.name,
                 "showing": self.showing(node), "enabled": self.enabled(node),
                 "focused": states.contains(self.ax.STATE_FOCUSED),
                 "bounds": [bounds.x, bounds.y, bounds.width, bounds.height],
                 "actions": actions}
        self.check("AX-description-after")
        return value

    def snapshot(self, name):
        values = [self.description(path, node) for path, node in self.nodes()]
        self.record("accessibility", name=name, nodes=values)
        return values

    def one(self, predicate, *, visible=True):
        matches = [(path, node) for path, node in self.nodes()
                   if predicate(node) and (not visible or self.showing(node))]
        require(len(matches) <= 1, "native target is ambiguous")
        return matches[0] if matches else None

    def button(self, name):
        return self.one(lambda node: node.getRole() == self.ax.ROLE_PUSH_BUTTON
                        and node.name == name and self.enabled(node))

    def field(self):
        require(self.arm == "after", "Before has no History query field")
        return self.one(lambda node: node.getRole() == self.ax.ROLE_ENTRY
                        and node.name == SEARCH and self.enabled(node)
                        and node.getState().contains(self.ax.STATE_EDITABLE))

    def native_focus(self):
        focus = int(self.tool(["xdotool", "getwindowfocus"]))
        require(focus == self.xid, "input focus left the exact owned window")
        # _NET_WM_PID is only a consistency observation; root proves ownership.
        require(int(self.tool(["xdotool", "getwindowpid", str(focus)])) == self.pid,
                "window PID observation differs from original owner binding")

    def geometry(self):
        text = self.tool(["xdotool", "getwindowgeometry", "--shell", str(self.xid)])
        data = dict(line.split("=", 1) for line in text.splitlines())
        require(int(data["WINDOW"], 0) == self.xid, "geometry belongs to another XID")
        return tuple(int(data[key]) for key in ("X", "Y", "WIDTH", "HEIGHT"))

    def viewport(self, size):
        require(tuple(size) in ((1440, 900), (900, 700)), "unplanned viewport")
        self.tool(["xdotool", "windowsize", "--sync", str(self.xid), *map(str, size)])
        self.tool(["xdotool", "windowmove", "--sync", str(self.xid), "0", "0"])
        self.poll(lambda: self.geometry() == (0, 0, *size), "viewport")
        self.size = tuple(size)
        self.record("viewport", observed_geometry=list(self.geometry()))

    def pointer_target(self, target):
        path, node = target
        item = self.description(path, node)
        x, y, width, height = item["bounds"]
        require(self.size is not None and item["showing"] and item["enabled"]
                and width > 0 and height > 0
                and 0 <= x and 0 <= y
                and x + width <= self.size[0] and y + height <= self.size[1],
                "target is clipped/outside owned viewport; real scrolling required")
        return item, (x + width // 2, y + height // 2)

    def fully_visible(self, target):
        item = self.description(*target)
        x, y, width, height = item["bounds"]
        return (self.size is not None and item["showing"] and item["enabled"]
                and width > 0 and height > 0 and 0 <= x and 0 <= y
                and x + width <= self.size[0] and y + height <= self.size[1])

    def activate(self, finder, *, keyboard=False):
        target = self.poll(finder, "activation-target")
        item, point = self.pointer_target(target)
        require(any(name.lower() == "click" for name in item["actions"]),
                "target exposes no native Click action")
        # Physical input, never queryAction.doAction or EditableText.setText.
        self.native_focus()
        if keyboard:
            self.check("AX-focus-before")
            require(target[1].queryComponent().grabFocus(), "target refused AX focus setup")
            self.check("AX-focus-after")
            self.poll(lambda: (current := finder()) is not None
                      and current[1].getState().contains(self.ax.STATE_FOCUSED), "focus-readback")
            self.native_focus()
            self.record("input", gesture="Return", target=item,
                        setup="AX grabFocus, separately disclosed; not Tab navigation")
            self.tool(["xdotool", "key", "Return"])
        else:
            self.record("input", gesture="pointer-click", target=item)
            self.tool(["xdotool", "mousemove", "--sync", *map(str, point), "click", "1"])

    def focus_field(self):
        target = self.poll(self.field, "search-field")
        item, point = self.pointer_target(target)
        self.native_focus()
        self.record("input", gesture="focus-query-pointer", target=item)
        self.tool(["xdotool", "mousemove", "--sync", *map(str, point), "click", "1"])
        self.poll(lambda: (current := self.field()) is not None
                  and current[1].getState().contains(self.ax.STATE_FOCUSED), "query-focus")

    def query_text(self):
        target = self.field()
        require(target is not None, "query field disappeared")
        self.check("AX-query-text-before")
        text = target[1].queryText().getText(0, -1)
        self.check("AX-query-text-after")
        return text

    def submit(self, text):
        require(self.arm == "after", "query/refine/clear unavailable on Before")
        self.focus_field()
        self.native_focus()
        self.record("input", gesture="replace-query", query=text)
        self.tool(["xdotool", "key", "ctrl+a"])
        if text:
            self.tool(["xdotool", "type", "--clearmodifiers", "--delay", "1", "--", text])
        else:
            self.tool(["xdotool", "key", "BackSpace"])
        self.poll(lambda: self.query_text() == text, "typed-query-readback")
        self.snapshot("query-draft-before-Enter")
        self.native_focus()
        require(self.field()[1].getState().contains(self.ax.STATE_FOCUSED), "query lost focus")
        self.record("input", gesture="query-submit-physical-Return", query=text)
        self.tool(["xdotool", "key", "Return"])
        # Submission resets list/detail in product. Do not assert old selection.

    def row_nodes(self):
        nodes = self.nodes()
        result = {}
        buttons = {}
        for path, node in nodes:
            self.check("AX-row-role")
            if node.getRole() == self.ax.ROLE_PUSH_BUTTON:
                buttons[path] = node
        labels_by_button = {path: [] for path in buttons}
        # Read each native name once, not once per row or per 25k data record.
        for path, node in nodes:
            self.check("AX-row-name")
            label = node.name or ""
            for length in range(len(path) + 1):
                ancestor = path[:length]
                if ancestor in labels_by_button:
                    labels_by_button[ancestor].append(label)
        # Current production rows are custom buttons. Names may be assembled
        # from child text; discover them instead of guessing a full AX label.
        for path, node in buttons.items():
            labels = labels_by_button[path]
            combined = "\n".join(labels)
            candidates = {label for label in labels if label in self.subjects}
            # Some bridges expose only a button's composed name. The generated
            # numeric subject identifies a candidate; verify all text below.
            for match in re.finditer(r"(?:small401|large25000) commit ([0-9]{6})", combined):
                index = int(match.group(1))
                if 1 <= index <= len(self.fixture.records):
                    candidates.add(row_text(self.fixture.records[index - 1])[0])
            if self.fixture.sibling:
                subject = row_text(self.fixture.sibling_record())[0]
                if subject in combined:
                    candidates.add(subject)
            for subject in candidates:
                record = self.subjects[subject]
                subject, abbreviated, author = row_text(record)
                if subject in combined and abbreviated in combined and author in combined:
                    require(record["oid"] not in result, "multiple AX buttons identify one fixture row")
                    result[record["oid"]] = (path, node)
        self.check("AX-row-match-after")
        return result

    def visible_outcome(self, name):
        values = self.snapshot(name)
        labels = [item["name"] for item in values if item["showing"]]
        rows = self.row_nodes()
        value = outcome(labels, len(rows), any(label in (PAGER, FIRST_PAGER) for label in labels))
        self.record("history-visible-outcome", name=name, **value,
                    recognized_fixture_row_oids=list(rows),
                    identity_scope="data-index association through subject/author/8-char label only")
        return value

    def scroll(self, finder, direction, count=1):
        require(direction in ("up", "down") and 1 <= count <= 25, "unplanned scroll")
        target = self.poll(finder, "scroll-region")
        item, point = self.pointer_target(target)
        self.native_focus()
        before = self.snapshot("scroll-before")
        self.record("input", gesture="wheel-" + direction, count=count, target=item)
        self.tool(["xdotool", "mousemove", "--sync", *map(str, point),
                   "click", "--repeat", str(count), "--delay", "20", "4" if direction == "up" else "5"])
        after = self.snapshot("scroll-after")
        self.record("scroll-observation", changed_AX_geometry=before != after,
                    claim="input sent; actual movement needs observed row geometry/frame review")

    def reveal(self, finder):
        # No fixed coordinates/assumed scrollbar. Wheel over an actually visible
        # History row. Bound attempts and fail rather than retrying activation.
        for _ in range(24):
            target = finder()
            if target is not None and self.fully_visible(target):
                return target
            direction = "down"
            if target is not None and self.description(*target)["bounds"][1] < 0:
                direction = "up"
            rows = self.row_nodes()
            anchors = [value for value in rows.values() if self.fully_visible(value)]
            require(anchors, "no visible History row to anchor real scroll")
            anchor = anchors[-1] if direction == "down" else anchors[0]
            self.scroll(lambda: anchor, direction, 5)
        raise Hold("History target did not become fully visible after bounded real scrolling")

    def hover(self, finder):
        item, point = self.pointer_target(self.poll(finder, "hover-target"))
        self.native_focus()
        self.record("input", gesture="hover", target=item)
        self.tool(["xdotool", "mousemove", "--sync", *map(str, point)])
        self.snapshot("hover-observed")

    def keyboard_navigation(self):
        # Record what the real fixture admits; global App KeyEvent is excluded.
        self.native_focus()
        for key in ("Tab", "shift+Tab", "Escape"):
            self.record("input", gesture="key", key=key)
            self.tool(["xdotool", "key", key])
            self.snapshot("keyboard-" + key.replace("+", "-"))

    def capture(self, name):
        require(re.fullmatch(r"[a-z0-9][a-z0-9-]{0,100}", name), "unsafe capture name")
        require(self.size is not None, "viewport has not been observed")
        self.native_focus()
        before = self.root.history_frame_observation(self.binding)
        require(before.geometry == (0, 0, *self.size), "capture geometry mismatch")
        # Owner must supply actual renderer/theme/scale observations, distinguish
        # configured versus observed, and bind them to this live PID/log/XID.
        require(before.renderer_observed is not None and before.theme_observed == "dark"
                and before.app_scale_configured == 1.0, "missing matched renderer/theme/scale evidence")
        artifacts = []
        prefix = f"{self.arm}-{self.fixture.dataset}-{name}-{self.size[0]}x{self.size[1]}"
        for kind, flags in (("screen", ["-screen"]), ("drawable", [])):
            png = self.root.reserve_history_artifact(self.binding, f"{prefix}-{kind}.png")
            self.tool(["import", *flags, "-window", str(self.xid), str(png)])
            self.check("frame-decode-before")
            with self.Image.open(png) as image:
                require(image.size == self.size, "frame not exact owned-window dimensions")
                require(image.format == "PNG", "native capture is not the retained original PNG")
                alpha_extrema = image.convert("RGBA").getchannel("A").getextrema()
                alpha = {"original_mode": image.mode, "original_bands": list(image.getbands()),
                         "original_transparency_metadata_present": "transparency" in image.info,
                         "decoded_alpha_extrema": list(alpha_extrema)}
                self.record("native-frame-alpha", kind=kind, original_png=str(png), **alpha)
                require(alpha_extrema == (255, 255),
                        "nonopaque native frame; retain original PNG without accepting a screenshot")
                rgb = image.convert("RGB")
                histogram = rgb.convert("L").histogram()
                levels = [level for level, count in enumerate(histogram) if count]
                metrics = {"size": list(image.size), "luminance_range": max(levels) - min(levels),
                           "at_least_16_colors": rgb.getcolors(maxcolors=15) is None,
                           "fraction_above_8": sum(histogram[9:]) / (image.width * image.height),
                           **alpha}
                require(metrics["luminance_range"] >= 16 and metrics["at_least_16_colors"]
                        and metrics["fraction_above_8"] > .01, "blank/unrendered native frame")
                self.check("frame-WebP-before")
                webp = self.root.reserve_history_artifact(self.binding, f"{prefix}-{kind}.webp")
                image.save(webp, lossless=True)
                self.check("frame-WebP-after")
            self.check("frame-decode-after")
            artifacts.append(self.root.retain_history_artifact(self.binding, png, webp, kind, metrics))
        after = self.root.history_frame_observation(self.binding)
        require(before == after, "capture ownership/geometry/theme/scale/backend changed")
        self.native_focus()
        self.record("native-frames", name=name, actual_binding=before.to_evidence(), artifacts=artifacts,
                    scope="owned-window rectangle in screen and direct drawable; no full-screen ratio")

    def ordinary_pagination(self):
        require(self.fixture.dataset == "small", "401 pager oracle needs small dataset")
        newest = self.fixture.newest()
        for count in (200, 400, 401):
            expected = {record["oid"] for record in newest[:count]}
            self.poll(lambda: set(self.row_nodes()) == expected, "ordinary-page-rows")
            self.visible_outcome("ordinary-" + str(count))
            if count < 401:
                finder = lambda: self.button(PAGER)
                self.reveal(finder)
                self.capture("ordinary-" + str(count))
                self.activate(finder)
                # Fast loading is permitted; no held-page/double-append proof.
                self.snapshot("pager-immediate-after-input")
            else:
                require(self.button(PAGER) is None, "short last page retained pager")
                self.capture("ordinary-401-complete")

    def inspect(self, record):
        oid = record["oid"]
        finder = lambda: self.row_nodes().get(oid)
        self.reveal(finder)
        self.activate(finder)
        self.poll(lambda: self.button("History"), "commit-detail")
        file_finder = lambda: self.button(record["changed_file"])
        self.poll(file_finder, "real-changed-file-row")
        self.capture("commit-detail")
        self.activate(file_finder, keyboard=True)
        self.snapshot("immutable-reader-opened")
        self.capture("immutable-reader")
        self.record("inspection-oracle", expected_full_oid=oid,
                    expected_public_content_sha256=record["content_sha256"],
                    actual_full_oid_and_reader_bytes="UNPROVEN_WITHOUT_NATIVE_READER_OBSERVER_OR_UX_REVIEW",
                    recovery_error="UNRUN_NO_READ_FAILURE_PRODUCER")
        self.activate(lambda: self.button("History"))
        self.poll(lambda: self.row_nodes().get(oid), "retained-row-return")

    def query_case(self, key):
        require(self.arm == "after" and key in self.fixture.queries, "unavailable query case")
        query = self.fixture.queries[key]
        self.submit(query)
        self.record("query-case", key=key, actual_query=query,
                    candidate_oracle_scope="literal index candidates; no deadline/limit promise",
                    expected_literal_candidate_oids=[item["oid"] for item in self.fixture.literal_candidates(query)])
        # A new query may complete immediately, be Limited with zero/known rows,
        # fail or still load. The following is observational, not an invented
        # callback/read-result oracle. Runtime owner/UX must resolve each case.
        self.snapshot("query-immediate-" + key.replace("_", "-"))

    def observe_query_result(self, key):
        """Wait for a real visible result shape, never inject a typed outcome.

        This is still not a generation/causality witness. In particular an
        unchanged matching old shape cannot prove which callback produced it.
        """
        require(self.arm == "after", "Before has no query results")
        query = self.fixture.queries[key]
        candidates = {record["oid"] for record in self.fixture.literal_candidates(query)}
        optional = {self.fixture.sibling["oid"]} if key == "full_sha_literal_or" else set()

        def observed():
            nodes = self.nodes()
            labels = [node.name for _, node in nodes if self.showing(node)]
            rows = self.row_nodes()
            draft = self.query_text()
            row_oids = set(rows)
            pager = PAGER in labels or FIRST_PAGER in labels
            self.record("query-read-observation", expected_query=query, actual_draft=draft,
                        visible_notices=[label for label in (LOADING, LIMIT, EMPTY, UNBORN) if label in labels],
                        recognized_fixture_row_oids=list(rows), pager_observed=pager,
                        applied_submit_or_callback_witness="UNSUPPLIED")
            # The draft is visible before Enter is applied. Prior/mixed renders
            # are ambiguous observations, not completed-new-query failures.
            # Retry reads only under the same original absolute deadline.
            if LOADING in labels or UNBORN in labels or draft != query:
                return None
            if LIMIT in labels:
                if EMPTY not in labels and row_oids <= candidates | optional and not pager:
                    return "limited-zero" if not rows else "limited-known"
                return None
            if EMPTY in labels:
                return "complete-empty" if not rows and not candidates and not pager else None
            if rows and row_oids <= candidates | optional:
                if len(rows) < 200 and not pager and candidates <= row_oids:
                    return "visible-short-result"
                if len(rows) == 200 and pager:
                    return "visible-full-page"
            return None

        shape = self.poll(observed, "query-visible-result")
        # A later AX read is another observation, not an atomic callback result
        # or grounds to reapply completeness assertions without that witness.
        self.snapshot("query-result-" + key.replace("_", "-"))
        self.capture("query-" + key.replace("_", "-"))
        self.record("query-result-shape", key=key, shape=shape,
                    meaning="visible UI only; no generation, limit cause, exhaustion or OID-resolution proof")
        return shape

    def unborn(self):
        require(self.fixture.dataset == "unborn", "wrong dataset")
        self.poll(lambda: any(node.name == UNBORN and self.showing(node)
                              for _, node in self.nodes()), "unborn-notice")
        require(not self.row_nodes() and self.button(PAGER) is None, "unborn exposes rows/pager")
        self.visible_outcome("unborn")
        self.capture("unborn")

    def unavailable_before(self):
        require(self.arm == "before", "wrong arm")
        labels = [node.name for _, node in self.nodes()]
        require(SEARCH not in labels and LIMIT not in labels, "Before unexpectedly contains feature controls")
        self.record("feature-unavailable", cases=["query", "refine", "full-SHA-search", "Limited"],
                    baseline="actual ordinary History on same dataset/viewport; never simulate new UI")

    def run_current_dataset(self):
        """Finite intended interactions on ONE already admitted live root.

        Does not create a root or clean one up. Parent must stage evidence and
        settle this root before the next arm/dataset. Nine capture-matrix cases
        are covered partially; explicit missing witnesses below prevent an
        assembled/native qualification verdict from this method alone.
        """
        self.viewport((1440, 900))
        if self.arm == "before":
            self.unavailable_before()
        if self.fixture.dataset == "unborn":
            self.unborn()
            self.viewport((900, 700))
            self.unborn()
            return
        initial = {record["oid"] for record in self.fixture.newest()[:200]}
        self.poll(lambda: set(self.row_nodes()) == initial, "initial-200-rows")
        self.visible_outcome("populated")
        self.capture("populated-wide")
        head = self.fixture.records[-1]
        row = lambda: self.row_nodes().get(head["oid"])
        self.hover(row)
        self.capture("populated-hover-wide")
        self.viewport((900, 700))
        self.capture("populated-narrow")
        self.keyboard_navigation()
        self.capture("keyboard-narrow")
        self.scroll(row, "down", 3)
        self.capture("scrolled-narrow")
        self.viewport((1440, 900))
        self.reveal(row)
        self.inspect(head)
        if self.fixture.dataset == "small":
            self.ordinary_pagination()
        if self.arm == "after":
            for key in ("message", "author", "late_only", "no_match", "full_sha_literal_or",
                        "refine_head", "clear"):
                self.query_case(key)
                shape = self.observe_query_result(key)
                self.record("matrix-query-observation", key=key, shape=shape,
                            limited_coverage="observed only when real caption appears; never forced")
        self.record("unqualified-subcases", stale_query="NO_ATTRIBUTABLE_DELAYED_CALLBACK_SEAM",
                    pager_owner="NO_HELD_PAGE_OR_DUPLICATE_COMPLETION_PRODUCER",
                    inspect_recovery="NO_REAL_READ_FAILURE_PRODUCER",
                    immutable_full_identity_and_bytes="NATIVE_READER_OBSERVER_OR_UX_REVIEW_REQUIRED",
                    performance="NO_ATTRIBUTABLE_FRAME_RESOURCE_OBSERVER",
                    native_UX_review="UNRUN", root_and_writer_settlement="ORIGINAL_OWNER_REQUIRED")


def main():
    # No flags, correlation UUID or self-authored receipt can cross this gate.
    raise SystemExit("HOLD: ROOT_INTERFACE_UNSUPPLIED; source methods only; no native imports or effects")


if __name__ == "__main__":
    main()
