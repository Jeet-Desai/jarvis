"""
qa.py — closed-loop visual QA for dev-agent output (P3).

Serve a built web app on an ephemeral localhost port, screenshot it at mobile +
desktop breakpoints with Playwright (capturing console / page errors), and (P3a+)
judge the result against a spec with a vision model. The QA loop lives here in the
Python sidecar; Rust stays the voice / gate / UI layer (see docs/P3-plan.md).

Deliberately free of any Jarvis / MCP coupling so it can be spiked + tested
standalone:

    uv run python qa.py ~/claude/jarvis-workspace/projects/landing-page-builder

Hard-won rules baked in (from the research dossier, docs/P3-research.md):
  - bind an EPHEMERAL port (socket.bind(('', 0))) and read it back — parallel
    agents would collide on a fixed port.
  - attach console / pageerror listeners BEFORE navigation, or early errors are lost.
  - a stale/empty `dist/` makes a static serve silently wrong — build first, guarded.
"""
from __future__ import annotations

import contextlib
import json
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from contextlib import contextmanager
from pathlib import Path
from typing import Iterator, Optional

# Screenshot breakpoints: (label, width, height). Mobile + desktop is the cheap,
# high-signal pair the research recommends — covers the responsiveness rubric line
# without a screenshot explosion.
BREAKPOINTS = [("mobile", 375, 812), ("desktop", 1280, 900)]

# Where built web apps emit their static output, most-specific first. If none of
# these exist we fall back to serving the worktree root (hand-written static site).
_OUTPUT_DIRS = ("dist", "build", "out", "public")


# ── serve ────────────────────────────────────────────────────────────────────

def _free_port() -> int:
    """Ask the OS for an unused TCP port, then release it. There's a tiny TOCTOU
    window before we re-bind it in http.server, but for localhost dev servers
    spawned one-at-a-time per worktree it's the standard, collision-safe idiom."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _wait_tcp(host: str, port: int, timeout: float = 20.0) -> bool:
    """Poll connect_ex until the server accepts, or timeout. No fixed sleep guess —
    return the instant the port is live."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.settimeout(0.5)
            if s.connect_ex((host, port)) == 0:
                return True
        time.sleep(0.1)
    return False


def _static_root(worktree: Path) -> Optional[Path]:
    """The directory to serve statically: a build-output dir with an index.html,
    else the worktree itself if it has one. None → can't serve statically."""
    for sub in _OUTPUT_DIRS:
        if (worktree / sub / "index.html").exists():
            return worktree / sub
    if (worktree / "index.html").exists():
        return worktree
    return None


def _pkg_manager(worktree: Path) -> str:
    if (worktree / "pnpm-lock.yaml").exists() and shutil.which("pnpm"):
        return "pnpm"
    if (worktree / "yarn.lock").exists() and shutil.which("yarn"):
        return "yarn"
    return "npm"


def _build_node(worktree: Path, timeout: float = 300.0) -> None:
    """Install (if needed) and `build` a node project so its output dir exists.
    Guarded: serving a stale/empty dist is a silent failure (research pitfall)."""
    pm = _pkg_manager(worktree)
    if not (worktree / "node_modules").exists():
        subprocess.run([pm, "install"], cwd=str(worktree), timeout=timeout, check=False)
    subprocess.run([pm, "run", "build"], cwd=str(worktree), timeout=timeout, check=False)


@contextmanager
def serve_app(worktree: Path, build: bool = True) -> Iterator[str]:
    """Serve a built web app from `worktree` on an ephemeral 127.0.0.1 port.

    Yields the base URL; tears the server down on exit (context-managed, so a
    capture crash never leaks a server). A static dir (root index.html or a
    dist/build/out/public output) is served with `python -m http.server`. A node
    project with no output yet is built first (guarded) then served statically.

    v1 scope is web apps that compile to static files (the cleanest serve story);
    long-running dev servers (`next start`) are a later milestone."""
    worktree = Path(worktree).expanduser().resolve()
    root = _static_root(worktree)
    if root is None and (worktree / "package.json").exists() and build:
        _build_node(worktree)
        root = _static_root(worktree)
    if root is None:
        raise RuntimeError(
            f"don't know how to serve {worktree}: no index.html and no "
            f"{'/'.join(_OUTPUT_DIRS)} output dir (build may have failed)"
        )

    port = _free_port()
    proc = subprocess.Popen(
        [sys.executable, "-m", "http.server", str(port), "--bind", "127.0.0.1"],
        cwd=str(root),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        if not _wait_tcp("127.0.0.1", port):
            raise RuntimeError(f"static server for {root} never came up on :{port}")
        yield f"http://127.0.0.1:{port}/"
    finally:
        proc.terminate()
        with contextlib.suppress(subprocess.TimeoutExpired):
            proc.wait(timeout=5)
        if proc.poll() is None:
            proc.kill()


# ── capture ──────────────────────────────────────────────────────────────────

# Injected BEFORE any page script runs. A full_page screenshot never scrolls, so
# the ubiquitous `opacity:0 → .is-visible` scroll-reveal pattern (IntersectionObserver)
# would be captured at its initial HIDDEN state — the judge then sees a half-empty
# page and (correctly, per its rubric) flags missing content. The landing-page spike
# hit exactly this. Relying on a scroll-pass to fire the observer is racy (the callback
# batches asynchronously; on short pages it lands after we've scrolled back). Instead we
# stub IntersectionObserver so every observed element reports intersecting immediately —
# the deterministic equivalent of "the user has scrolled past everything", which is the
# correct QA semantics (we want to judge the fully-revealed page). Also forces IO-based
# lazy media to load.
_QA_INIT_SCRIPT = """
(() => {
  const RealIO = window.IntersectionObserver;
  class QAObserver {
    constructor(cb) { this._cb = cb; }
    observe(el) {
      try {
        this._cb([{
          isIntersecting: true, intersectionRatio: 1, target: el,
          time: 0, boundingClientRect: el.getBoundingClientRect(),
          intersectionRect: el.getBoundingClientRect(), rootBounds: null,
        }], this);
      } catch (e) { /* never break the page */ }
    }
    unobserve() {} disconnect() {} takeRecords() { return []; }
  }
  window.IntersectionObserver = QAObserver;
  window.__qaRealIO = RealIO;
})();
"""

# Disable transitions/animations so the screenshot captures final states crisply
# (no mid-fade blur) rather than whatever frame the capture happened to land on.
_REDUCE_MOTION_CSS = """
*, *::before, *::after {
  transition-duration: 0s !important; transition-delay: 0s !important;
  animation-duration: 0s !important; animation-delay: 0s !important;
  scroll-behavior: auto !important;
}
"""


async def _trigger_lazy_content(page) -> None:
    """Quick scroll to the bottom and back to nudge NATIVE lazy media
    (`loading="lazy"` images) into loading. Reveal animations are already handled
    deterministically by `_QA_INIT_SCRIPT`; this is just belt-and-suspenders for
    images that load on viewport proximity."""
    with contextlib.suppress(Exception):
        await page.evaluate(
            """async () => {
                await new Promise((resolve) => {
                    let y = 0;
                    const step = () => {
                        window.scrollTo(0, y);
                        y += Math.max(window.innerHeight, 200);
                        if (y < document.body.scrollHeight + window.innerHeight) {
                            setTimeout(step, 40);
                        } else { window.scrollTo(0, 0); resolve(); }
                    };
                    step();
                });
            }"""
        )


async def capture(
    url: str,
    spec: str = "",
    out_dir: Optional[Path] = None,
    sentinel: Optional[str] = None,
) -> dict:
    """Navigate `url` headless and screenshot it at each breakpoint, capturing
    console errors + uncaught page errors (attached BEFORE navigation).

    Returns {url, shots:[{label,path,width,height}], console_errors:[...],
    page_errors:[...], a11y:[...], out_dir}. `sentinel` is an optional CSS
    selector to wait for (SPA "ready" marker) on top of networkidle."""
    from playwright.async_api import async_playwright

    out = Path(out_dir) if out_dir else Path(tempfile.mkdtemp(prefix="jarvis-qa-"))
    out.mkdir(parents=True, exist_ok=True)

    shots: list[dict] = []
    console_errors: list[str] = []
    page_errors: list[str] = []

    async with async_playwright() as p:
        browser = await p.chromium.launch()
        try:
            for label, w, h in BREAKPOINTS:
                ctx = await browser.new_context(viewport={"width": w, "height": h})
                page = await ctx.new_page()
                # init script runs before any page JS → deterministically reveals
                # scroll-triggered content (see _QA_INIT_SCRIPT).
                await page.add_init_script(_QA_INIT_SCRIPT)
                # Listeners MUST be attached before goto, or first-paint errors are missed.
                page.on(
                    "console",
                    lambda msg: console_errors.append(msg.text) if msg.type == "error" else None,
                )
                page.on("pageerror", lambda exc: page_errors.append(str(exc)))
                try:
                    await page.goto(url, wait_until="networkidle", timeout=20_000)
                except Exception as e:  # noqa: BLE001 - record, still try to shoot
                    page_errors.append(f"navigation: {e!r}")
                if sentinel:
                    with contextlib.suppress(Exception):
                        await page.wait_for_selector(sentinel, timeout=5_000)
                with contextlib.suppress(Exception):
                    await page.add_style_tag(content=_REDUCE_MOTION_CSS)
                await _trigger_lazy_content(page)
                shot = out / f"{label}.png"
                await page.screenshot(path=str(shot), full_page=True)
                shots.append({"label": label, "path": str(shot), "width": w, "height": h})
                await ctx.close()
        finally:
            await browser.close()

    return {
        "url": url,
        "shots": shots,
        "console_errors": _dedup(console_errors),
        "page_errors": _dedup(page_errors),
        "a11y": [],  # axe-playwright-python hook (P3c)
        "out_dir": str(out),
    }


def _dedup(xs: list[str]) -> list[str]:
    """Drop duplicate error strings (the same console error fires per breakpoint)
    while preserving first-seen order."""
    seen: set[str] = set()
    out: list[str] = []
    for x in xs:
        if x not in seen:
            seen.add(x)
            out.append(x)
    return out


# ── standalone spike ─────────────────────────────────────────────────────────

if __name__ == "__main__":
    import argparse
    import asyncio

    ap = argparse.ArgumentParser(description="Serve + screenshot a web app (P3 spike).")
    ap.add_argument("worktree", help="path to the built web app / worktree")
    ap.add_argument("--spec", default="", help="(unused in the spike) QA spec text")
    ap.add_argument("--out", default="", help="screenshot output dir")
    ap.add_argument("--no-build", action="store_true", help="skip the node build step")
    args = ap.parse_args()

    async def _main() -> None:
        with serve_app(Path(args.worktree), build=not args.no_build) as url:
            print(f"▶ serving {args.worktree} at {url}")
            res = await capture(url, args.spec, Path(args.out) if args.out else None)
        print(json.dumps(res, indent=2))
        print(f"\n✓ {len(res['shots'])} screenshots → {res['out_dir']}")
        print(f"  console_errors: {len(res['console_errors'])}, "
              f"page_errors: {len(res['page_errors'])}")

    asyncio.run(_main())
