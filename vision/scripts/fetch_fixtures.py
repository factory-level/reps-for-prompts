#!/usr/bin/env python3
"""Download permissively-licensed workout clips for the cvvideo test suite.

Sources are Wikimedia Commons files with stable upload.wikimedia.org URLs.
Expected rep counts in MANIFEST were established by running `reps analyze`
and sanity-checking against the footage; tolerances absorb pose-model drift.

Usage: uv run python scripts/fetch_fixtures.py
"""

from __future__ import annotations

import argparse
import hashlib
import os
import tempfile
import json
import time
import urllib.error
import urllib.request
from pathlib import Path

FIXTURES = Path(__file__).parent.parent / "tests" / "fixtures" / "videos"

CLIPS = [
    {
        "file": "interval_pushups.webm",
        "url": "https://upload.wikimedia.org/wikipedia/commons/9/98/Interval_Push-ups.webm",
        "license": "CC BY-SA 4.0",
        "source": "https://commons.wikimedia.org/wiki/File:Interval_Push-ups.webm",
        "exercise": "pushup",
        "expected_reps": None,  # filled below after labeling
        "tolerance": None,
    },
    {
        "file": "squat_demo.webm",
        "url": "https://upload.wikimedia.org/wikipedia/commons/5/5c/Squat_-_exercise_demonstration_video.webm",
        "license": "CC BY 3.0",
        "source": "https://commons.wikimedia.org/wiki/File:Squat_-_exercise_demonstration_video.webm",
        "exercise": "squat",
        "expected_reps": None,
        "tolerance": None,
    },
    {
        "file": "kb_racked_squats_side.webm",
        "url": "https://upload.wikimedia.org/wikipedia/commons/9/93/Kettlebell_Racked_Squats_%28side_view%29.webm",
        "license": "CC BY-SA 4.0",
        "source": "https://commons.wikimedia.org/wiki/File:Kettlebell_Racked_Squats_(side_view).webm",
        "exercise": "squat",
        "expected_reps": None,
        "tolerance": None,
    },
]

# Labeled expectations, verified against the footage (interval_pushups was
# additionally confirmed by inspecting the full elbow-angle trace: exactly
# six dips to ~31 degrees). Keyed by file name.
EXPECTATIONS = {
    "interval_pushups.webm": {"expected_reps": 6, "tolerance": 1},
    "squat_demo.webm": {"expected_reps": 2, "tolerance": 1},
    "kb_racked_squats_side.webm": {"expected_reps": 6, "tolerance": 1},
}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--benchmark', action='store_true', help='Also download the reviewed temporal benchmark videos (including curls)')
    args = parser.parse_args()
    FIXTURES.mkdir(parents=True, exist_ok=True)
    benchmark = json.loads((FIXTURES / 'benchmark.json').read_text())['clips']
    sources = {entry['file']: entry for entry in benchmark}
    selected = {clip['file']: clip for clip in CLIPS}
    if args.benchmark:
        selected.update(sources)
    manifest = []
    for clip in selected.values():
        target = FIXTURES / clip["file"]
        expected_hash = sources.get(clip['file'], {}).get('sha256')
        if target.exists():
            if expected_hash and hashlib.sha256(target.read_bytes()).hexdigest() != expected_hash:
                raise ValueError(f'Checksum mismatch: {target}; remove the damaged fixture and retry')
            print(f"already present: {clip['file']}")
        else:
            print(f"downloading {clip['file']} ...")
            req = urllib.request.Request(
                # Wikimedia throttles (HTTP 429) user agents that do not identify
                # the project with a URL: https://meta.wikimedia.org/wiki/User-Agent_policy
                clip["url"], headers={"User-Agent": "rfp-fixtures/0.1 (https://github.com/factory-level/reps-for-prompts)"}
            )
            for attempt in range(5):
                try:
                    fd, temporary = tempfile.mkstemp(dir=FIXTURES, suffix='.part')
                    try:
                        digest = hashlib.sha256()
                        total = 0
                        with os.fdopen(fd, 'wb') as output, urllib.request.urlopen(req, timeout=60) as resp:
                            while chunk := resp.read(1024 * 1024):
                                total += len(chunk)
                                if total > 128 * 1024 * 1024:
                                    raise ValueError('Fixture exceeds 128 MiB limit')
                                digest.update(chunk)
                                output.write(chunk)
                        if expected_hash and digest.hexdigest() != expected_hash:
                            raise ValueError(f'Download checksum mismatch: {target}')
                        os.replace(temporary, target)
                    finally:
                        if os.path.exists(temporary):
                            os.unlink(temporary)
                    break
                except urllib.error.HTTPError as e:
                    if e.code != 429 or attempt == 4:
                        raise
                    delay = 15 * (attempt + 1)
                    print(f"  rate-limited; retrying in {delay}s")
                    time.sleep(delay)
            print(f"  -> {target} ({target.stat().st_size / 1e6:.1f} MB)")
            time.sleep(5)  # be polite between downloads
        if clip["file"] in EXPECTATIONS:
            original = next(e for e in CLIPS if e["file"] == clip["file"])
            manifest.append({**original, **EXPECTATIONS[clip["file"]]})
    (FIXTURES / "manifest.json").write_text(json.dumps(manifest, indent=2))
    print(f"manifest: {FIXTURES / 'manifest.json'}")


if __name__ == "__main__":
    main()
