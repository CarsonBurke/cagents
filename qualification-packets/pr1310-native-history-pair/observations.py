"""PR1310 fixture bindings and observation vocabulary; no native dependencies.

This validates public builder bytes supplied by the future original owner. It
does not read a repository, generate data, admit execution or certify custody.
"""

from dataclasses import dataclass
import hashlib
import json
import re


SCHEMA = "pr1310-public-git-fixture-v1"
BUILDER_SHA256 = "77b0694f9b3e2f7cec55ba4bbd6e2a534e5f661c5340cef342ad2669f11d5b01"
BUILDER_PAIR = {
    "before": "a3221af83d56663f0dcbef90f67b30ad6d9fe53d",
    "after": "efa9288b3fd79a7214307a11b1c6d50c6cde3630",
}
ADAPTERS = {
    "before": ("146ad67b926672367817d06897e4cb9f55176bbd",
               "c61e06cd488a6801863cc48d2de429c8f993ba9d"),
    "after": ("3ba686a8bd2d9ee500885c0ec5a2c5065f1c5dce",
              "124bd8583a75133bb4ed564ef829a154d123c4ed"),
}
SEARCH = "Message, author or SHA — Enter to search"
LIMIT = ("Search reached its limit; more matches may exist. "
         "Refine the query to search further.")
EMPTY = "No commits match this message, author or SHA."
UNBORN = "No commits yet."
LOADING = "Loading commits…"
PAGER = "Load 200 more commits"
FIRST_PAGER = "Load 200 commits"
OID = re.compile(r"[0-9a-f]{40}\Z")


class Hold(RuntimeError):
    """Evidence missing/ambiguous; never convert this into a product pass."""


def require(condition, message):
    if not condition:
        raise Hold(message)


def digest(data):
    return hashlib.sha256(data).hexdigest()


@dataclass(frozen=True)
class Fixture:
    dataset: str
    manifest_sha256: str
    index_sha256: str | None
    root: str
    head_oid: str | None
    records: tuple
    queries: dict
    sibling: dict | None

    @classmethod
    def from_owned_bytes(cls, manifest_bytes, index_bytes, dataset):
        """Caller must separately retain/check owned manifest/index/Git custody.

        Original builder-pair fields are schema provenance, not the native
        adapter pair. The latter is pinned independently in ADAPTERS/plan.
        """
        require(len(manifest_bytes) <= 4 * 1024 * 1024, "manifest exceeds adapter bound")
        data = json.loads(manifest_bytes)
        require(data["schema"] == SCHEMA, "fixture schema mismatch")
        require(data["source_pair"] == BUILDER_PAIR, "original builder provenance mismatch")
        require(data["status"] == "GENERATED_GIT_DATA_ONLY_NOT_NATIVE_QUALIFICATION",
                "builder incomplete; source DATA package is not generated Git data")
        require(data["builder_sha256"] == BUILDER_SHA256, "builder bytes mismatch")
        require(dataset in ("small", "large", "unborn"), "unknown dataset")
        item = data[dataset]
        root = data["root"]
        directory = {"small": "small401", "large": "large25000", "unborn": "unborn"}[dataset]
        require(item["root"] == root + "/" + directory, "dataset escaped manifest root")
        require(item["branch"] == "main", "unexpected fixture branch")
        if dataset == "unborn":
            require(item["head_oid"] is None and item["commits"] == 0,
                    "unborn declaration mismatch")
            require(index_bytes is None, "unborn has no generated commit index")
            return cls(dataset, digest(manifest_bytes), None, item["root"], None, (), {}, None)
        count = 401 if dataset == "small" else 25_000
        require(item["mainline_commits"] == count and item["all_commits"] == count + 1,
                "fixture cardinality declaration mismatch")
        require(OID.fullmatch(item["head_oid"]) is not None, "invalid generated HEAD")
        require(index_bytes is not None and len(index_bytes) <= 64 * 1024 * 1024,
                "missing or oversized actual builder index")
        require(digest(index_bytes) == item["commit_index_sha256"], "commit index bytes mismatch")
        records = tuple(json.loads(line) for line in index_bytes.splitlines())
        require(len(records) == count, "commit index cardinality mismatch")
        for position, record in enumerate(records, 1):
            require(record["oldest_first_index1"] == position
                    and record["head_first_index0"] == count - position,
                    "index sequence mismatch")
            require(OID.fullmatch(record["oid"]) is not None, "invalid index OID")
            require(record["parent_oid"] == (records[position - 2]["oid"] if position > 1 else None),
                    "index parent chain mismatch")
            require(digest(record["content"].encode("utf-8")) == record["content_sha256"],
                    "immutable inspection bytes mismatch")
            require(record["changed_file"] == "inspection.txt", "unexpected inspection path")
        require(len({record["oid"] for record in records}) == count, "duplicate index OID")
        require(records[-1]["oid"] == item["head_oid"], "index HEAD mismatch")
        sibling = item["sibling"]
        require(OID.fullmatch(sibling["oid"]) is not None
                and sibling["oid"] not in {record["oid"] for record in records},
                "invalid sibling declaration")
        require(item["queries"]["full_sha_literal_or"] == sibling["oid"],
                "full-SHA query must be the generated sibling OID")
        return cls(dataset, digest(manifest_bytes), digest(index_bytes), item["root"],
                   item["head_oid"], records, item["queries"], sibling)

    def newest(self):
        return tuple(reversed(self.records))

    def sibling_record(self):
        if self.sibling is None:
            return None
        # write_import's exact public sibling recipe; no Git object is created.
        name = "small401" if self.dataset == "small" else "large25000"
        return {**self.sibling, "message": name + " sibling inspection",
                "author": "Public Sibling Author",
                "content_sha256": digest(self.sibling["content"].encode("utf-8"))}

    def literal_candidates(self, query):
        """Data oracle, NOT a prediction of completion/limit/deadline/resolution."""
        needle = query.lower()
        return tuple(record for record in self.newest()
                     if needle in record["message"].lower()
                     or needle in record["author"].lower()
                     or record["oid"].lower().startswith(needle))


def row_text(record):
    # Discover actual button/descendant labels; this is NOT a guessed exact AX name.
    return record["message"].splitlines()[0], record["oid"][:8], record["author"]


def outcome(labels, row_count, pager_present):
    """Visible observations alone never identify request generation/full OID."""
    limited = LIMIT in labels
    require(not (limited and pager_present), "Limited state incorrectly exposes a pager")
    require(not (EMPTY in labels and row_count), "no-match notice coexists with known rows")
    require(not (UNBORN in labels and row_count), "unborn notice coexists with known rows")
    return {
        "limited_caption_observed": limited,
        "complete_empty_caption_observed": EMPTY in labels,
        "unborn_caption_observed": UNBORN in labels,
        "loading_caption_observed": LOADING in labels,
        "pager_observed": pager_present,
        "recognized_AX_rows": row_count,
        "full_result_identity": "UNPROVEN_BY_ABBREVIATED_UI_LABELS",
        "request_generation_HEAD_page_causality": "UNOBSERVED_NO_PRODUCER_LEDGER",
        "work_charged_and_limit_cause": "UNOBSERVED",
    }
