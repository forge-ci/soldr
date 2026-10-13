"""Validated records consumed by the README benchmark renderers."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True)
class JsonObject:
    """A decoded JSON object at the parse boundary; empty when not an object."""

    fields: dict  # raw JSON object, keys normalized to str

    def get(self, key: str) -> Any:
        return self.fields.get(key)


def object_field(value: Any) -> JsonObject:
    if not isinstance(value, dict):
        return JsonObject({})
    return JsonObject({str(key): item for key, item in value.items()})


def text_field(value: Any, default: str = "") -> str:
    return value if isinstance(value, str) else default


def number_field(value: Any) -> int | float | None:
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return value
    return None


@dataclass(frozen=True)
class CanaryTimings:
    cold: int | float | None
    warm: int | float | None
    from_warm_zccache: int | float | None
    cross_verb: int | float | None
    touch_no_change: int | float | None
    worktree_share: int | float | None

    @classmethod
    def from_json(cls, value: Any) -> CanaryTimings:
        row = object_field(value)
        return cls(
            cold=number_field(row.get("cargo-build-medium-cold")),
            warm=number_field(row.get("cargo-build-medium-warm")),
            from_warm_zccache=number_field(
                row.get("cargo-build-medium-from-warm-zccache")
            ),
            cross_verb=number_field(row.get("cargo-check-medium-cross-verb")),
            touch_no_change=number_field(row.get("touch-no-change-medium-warm")),
            worktree_share=number_field(row.get("worktree-share-medium-warm")),
        )

    def get(self, name: str) -> int | float | None:
        match name:
            case "cargo-build-medium-cold":
                return self.cold
            case "cargo-build-medium-warm":
                return self.warm
            case "cargo-build-medium-from-warm-zccache":
                return self.from_warm_zccache
            case "cargo-check-medium-cross-verb":
                return self.cross_verb
            case "touch-no-change-medium-warm":
                return self.touch_no_change
            case "worktree-share-medium-warm":
                return self.worktree_share
            case _:
                raise ValueError(f"unknown canary: {name}")


@dataclass(frozen=True)
class HistoryRow:
    canaries: CanaryTimings

    @classmethod
    def from_json(cls, value: Any) -> HistoryRow:
        return cls(CanaryTimings.from_json(object_field(value).get("canaries")))


@dataclass(frozen=True)
class ComparisonResult:
    benchmark: str
    scenario_key: str
    tool: str
    wall_ms: int | float | None
    cache_bytes: int | float | None

    @classmethod
    def from_json(cls, value: Any) -> ComparisonResult:
        row = object_field(value)
        return cls(
            benchmark=text_field(row.get("benchmark")),
            scenario_key=text_field(row.get("scenario_key")),
            tool=text_field(row.get("tool")),
            wall_ms=number_field(row.get("wall_ms")),
            cache_bytes=number_field(row.get("cache_bytes")),
        )


@dataclass(frozen=True)
class ToolLabel:
    key: str
    label: str

    @classmethod
    def from_json(cls, value: Any) -> ToolLabel:
        row = object_field(value)
        return cls(text_field(row.get("key")), text_field(row.get("label")))


@dataclass(frozen=True)
class ComparisonDocument:
    ran_at: str
    soldr_version: str
    sccache_version: str
    rustc_version: str
    tools: list[ToolLabel]
    results: list[ComparisonResult]

    @classmethod
    def from_json(cls, value: Any) -> ComparisonDocument:
        row = object_field(value)
        tools = row.get("tools")
        results = row.get("results")
        return cls(
            ran_at=text_field(row.get("ran_at"), "unknown"),
            soldr_version=text_field(row.get("soldr_version")),
            sccache_version=text_field(row.get("sccache_version")),
            rustc_version=text_field(row.get("rustc_version")),
            tools=[ToolLabel.from_json(item) for item in tools]
            if isinstance(tools, list)
            else [],
            results=[ComparisonResult.from_json(item) for item in results]
            if isinstance(results, list)
            else [],
        )
