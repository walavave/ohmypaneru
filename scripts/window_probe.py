#!/usr/bin/env python3
"""Standalone macOS window probe.

This script is intentionally outside Paneru's runtime. It dumps what public
macOS APIs can see without changing any Paneru ECS state.

Usage:
  python3 scripts/window_probe.py
  python3 scripts/window_probe.py --only terminal
  python3 scripts/window_probe.py --mode cg
  python3 scripts/window_probe.py --mode system-events
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from typing import Any


def quartz_windows() -> dict[str, Any]:
    try:
        import Quartz  # type: ignore
    except Exception as exc:
        return {
            "source": "core_graphics",
            "error": (
                "PyObjC Quartz is not importable. Install pyobjc-framework-Quartz "
                "or use --mode system-events."
            ),
            "detail": str(exc),
        }

    options = (
        Quartz.kCGWindowListOptionOnScreenOnly
        | Quartz.kCGWindowListExcludeDesktopElements
    )
    raw_windows = Quartz.CGWindowListCopyWindowInfo(options, Quartz.kCGNullWindowID) or []
    windows: list[dict[str, Any]] = []

    for item in raw_windows:
        bounds = item.get("kCGWindowBounds") or {}
        windows.append(
            {
                "window_id": item.get("kCGWindowNumber"),
                "owner_name": item.get("kCGWindowOwnerName"),
                "name": item.get("kCGWindowName"),
                "pid": item.get("kCGWindowOwnerPID"),
                "layer": item.get("kCGWindowLayer"),
                "onscreen": item.get("kCGWindowIsOnscreen"),
                "alpha": item.get("kCGWindowAlpha"),
                "sharing_state": item.get("kCGWindowSharingState"),
                "bounds": {
                    "x": bounds.get("X"),
                    "y": bounds.get("Y"),
                    "width": bounds.get("Width"),
                    "height": bounds.get("Height"),
                },
            }
        )

    return {
        "source": "core_graphics",
        "windows": windows,
    }


SYSTEM_EVENTS_SCRIPT = r'''
ObjC.import('stdlib');

function valueOrNull(fn) {
  try {
    var value = fn();
    if (value === undefined) return null;
    return value;
  } catch (_) {
    return null;
  }
}

function stringify(value) {
  if (value === null || value === undefined) return null;
  try {
    return value.toString();
  } catch (_) {
    return null;
  }
}

function arrayValue(value) {
  if (value === null || value === undefined) return null;
  try {
    return value();
  } catch (_) {
    return null;
  }
}

function windowRecord(process, win) {
  return {
    process_name: stringify(valueOrNull(function () { return process.name(); })),
    bundle_id: stringify(valueOrNull(function () { return process.bundleIdentifier(); })),
    pid: valueOrNull(function () { return process.unixId(); }),
    frontmost: valueOrNull(function () { return process.frontmost(); }),
    window_name: stringify(valueOrNull(function () { return win.name(); })),
    role: stringify(valueOrNull(function () { return win.role(); })),
    subrole: stringify(valueOrNull(function () { return win.subrole(); })),
    description: stringify(valueOrNull(function () { return win.description(); })),
    focused: valueOrNull(function () { return win.focused(); }),
    visible: valueOrNull(function () { return win.visible(); }),
    position: arrayValue(valueOrNull(function () { return win.position; })),
    size: arrayValue(valueOrNull(function () { return win.size; }))
  };
}

var systemEvents = Application('System Events');
var records = [];
var processes = systemEvents.applicationProcesses();

for (var i = 0; i < processes.length; i++) {
  var process = processes[i];
  var windows = valueOrNull(function () { return process.windows(); }) || [];
  for (var j = 0; j < windows.length; j++) {
    records.push(windowRecord(process, windows[j]));
  }
}

JSON.stringify({
  source: 'system_events_accessibility',
  windows: records
});
'''


def system_events_windows() -> dict[str, Any]:
    result = subprocess.run(
        ["/usr/bin/osascript", "-l", "JavaScript", "-e", SYSTEM_EVENTS_SCRIPT],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return {
            "source": "system_events_accessibility",
            "error": result.stderr.strip() or result.stdout.strip(),
            "returncode": result.returncode,
        }
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        return {
            "source": "system_events_accessibility",
            "error": "failed to parse osascript JSON",
            "detail": str(exc),
            "stdout": result.stdout,
            "stderr": result.stderr,
        }


def filter_windows(payload: dict[str, Any], needle: str | None) -> dict[str, Any]:
    if not needle:
        return payload

    needle = needle.lower()
    windows = payload.get("windows")
    if not isinstance(windows, list):
        return payload

    def matches(window: dict[str, Any]) -> bool:
        haystack = " ".join(
            str(value).lower()
            for value in window.values()
            if value is not None and not isinstance(value, (dict, list))
        )
        return needle in haystack

    filtered = dict(payload)
    filtered["windows"] = [
        window for window in windows if isinstance(window, dict) and matches(window)
    ]
    filtered["filter"] = needle
    return filtered


def main() -> int:
    parser = argparse.ArgumentParser(description="Dump macOS windows outside Paneru.")
    parser.add_argument(
        "--mode",
        choices=("all", "cg", "system-events"),
        default="all",
        help="Window API to probe.",
    )
    parser.add_argument(
        "--only",
        help="Case-insensitive substring filter applied to scalar window fields.",
    )
    args = parser.parse_args()

    payloads: list[dict[str, Any]] = []
    if args.mode in ("all", "cg"):
        payloads.append(filter_windows(quartz_windows(), args.only))
    if args.mode in ("all", "system-events"):
        payloads.append(filter_windows(system_events_windows(), args.only))

    print(json.dumps(payloads if args.mode == "all" else payloads[0], ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
