#!/usr/bin/env python3
"""Static checks for the design invariants of discord-ext-songbird 0.6 (docs/design.md §1, §4, §5, §13).

Run from anywhere: ``python scripts/check_invariants.py``. Exits non-zero and lists every violation.

Checked:

* G1 / I1  ``crates/sbcore`` has no pyo3-family dependency, direct or transitive (``Cargo.lock``).
* I1       No struct/enum field or static in the binding crate holds ``Py<…>``, ``Bound``,
           ``PyObject``, ``PyBuffer``, ``PyUntypedBuffer``, ``PyBackedBytes`` (allow-list:
           ``static EXC_TYPES`` in ``src/errors.rs``).
* G2 / I2  No ``Python::attach``, ``try_attach`` or ``with_gil`` anywhere in Rust code.
* §4.4     No ``__traverse__`` / ``__clear__`` in the binding crate.
* §5.6     Every ``#[pyclass(…)]`` is ``frozen``; the module is ``gil_used = false``.
* I7       No public sbcore function returns a lock guard (``lock_checked`` is the documented
           exemption: it takes a mutex the caller already owns).
* §5.4     The four ``WakeHub`` operations on ``pending`` / the ready bits use ``SeqCst`` only.
* hard     No ``todo!()`` / ``unimplemented!()``.
* §2.3     pyo3 without ``experimental-async`` / ``extension-module``; dropped crates stay dropped;
           ``clippy::await_holding_lock`` is denied; the binding denies ``unwrap_used``/``expect_used``.
"""

from __future__ import annotations

import re
import sys
from collections.abc import Iterator
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - Python < 3.11
    sys.exit("check_invariants.py needs Python >= 3.11 (tomllib)")

ROOT = Path(__file__).resolve().parent.parent
SBCORE = ROOT / "crates" / "sbcore"
BINDING_SRC = ROOT / "src"

PYO3_FAMILY = re.compile(r"^(pyo3|pyo3[-_].*|numpy|pythonize|inline-python|cpython|python3-sys|.*[-_]pyo3)$")
DROPPED_CRATES = {
    "pyo3-async-runtimes",
    "pyo3-log",
    "dashmap",
    "arrow",
    "tokio-stream",
    "async-stream",
    "futures",
    "pin-project-lite",
    "env_logger",
    "uuid",
}
PY_HOLDING_TYPES = re.compile(
    r"\b(Py\s*<|Bound\s*<|Borrowed\s*<|PyObject\b|PyBuffer\b|PyUntypedBuffer\b|PyBackedBytes\b|PyRef\s*<|PyRefMut\s*<)"
)
LOCK_GUARD = re.compile(r"\b\w*(MutexGuard|RwLock\w*Guard|ReentrantMutexGuard|MappedMutexGuard)\b")
ATTACH = re.compile(r"\bPython::attach\b|\btry_attach\b|\bwith_gil\b")
WAKE_OPS = re.compile(r"\b(pending|ready_word)\s*\.\s*(fetch_\w+|swap|store|load|compare_exchange\w*)\s*\(")
NON_SEQCST = re.compile(r"\b(Relaxed|Acquire|Release|AcqRel)\b")

errors: list[str] = []


def fail(msg: str) -> None:
    errors.append(msg)


def rust_files(base: Path) -> Iterator[Path]:
    yield from sorted(p for p in base.rglob("*.rs") if "target" not in p.parts)


def strip_comments(src: str) -> str:
    """Removes // and /* */ comments while keeping string literals and line numbers intact."""
    out: list[str] = []
    i, n = 0, len(src)
    in_str = False
    while i < n:
        c = src[i]
        if in_str:
            out.append(c)
            if c == "\\" and i + 1 < n:
                out.append(src[i + 1])
                i += 2
                continue
            if c == '"':
                in_str = False
            i += 1
        elif c == '"':
            in_str = True
            out.append(c)
            i += 1
        elif c == "'" and i + 2 < n and src[i + 2] == "'":  # char literal like '"'
            out.append(src[i : i + 3])
            i += 3
        elif src.startswith("//", i):
            j = src.find("\n", i)
            i = n if j < 0 else j
        elif src.startswith("/*", i):
            j = src.find("*/", i + 2)
            end = n if j < 0 else j + 2
            out.append("\n" * src.count("\n", i, end))
            i = end
        else:
            out.append(c)
            i += 1
    return "".join(out)


def line_of(src: str, pos: int) -> int:
    return src.count("\n", 0, pos) + 1


def matching(src: str, open_pos: int) -> int:
    """Index just past the bracket matching the one at ``open_pos``."""
    pairs = {"{": "}", "(": ")", "[": "]"}
    opener = src[open_pos]
    closer = pairs[opener]
    depth = 0
    for i in range(open_pos, len(src)):
        if src[i] == opener:
            depth += 1
        elif src[i] == closer:
            depth -= 1
            if depth == 0:
                return i + 1
    return len(src)


def rel(p: Path) -> str:
    return str(p.relative_to(ROOT))


# ── dependencies ─────────────────────────────────────────────────────────────────────────────────────


def manifest_deps(manifest: dict) -> Iterator[tuple[str, str, dict | str]]:
    for table in ("dependencies", "dev-dependencies", "build-dependencies"):
        for name, spec in manifest.get(table, {}).items():
            yield table, name, spec
    for target, tables in manifest.get("target", {}).items():
        for table in ("dependencies", "dev-dependencies", "build-dependencies"):
            for name, spec in tables.get(table, {}).items():
                yield f"target.{target}.{table}", name, spec


def real_name(name: str, spec: dict | str) -> str:
    if isinstance(spec, dict) and "package" in spec:
        return spec["package"]
    return name


def check_sbcore_manifest() -> None:
    manifest = tomllib.loads((SBCORE / "Cargo.toml").read_text())
    for table, name, spec in manifest_deps(manifest):
        if PYO3_FAMILY.match(real_name(name, spec)):
            fail(f"crates/sbcore/Cargo.toml [{table}] depends on pyo3-family crate {name!r} (G1)")


def check_lockfile() -> None:
    lock_path = ROOT / "Cargo.lock"
    if not lock_path.exists():
        fail("Cargo.lock is missing; run `cargo fetch`")
        return
    lock = tomllib.loads(lock_path.read_text())
    packages = lock.get("package", [])
    by_name: dict[str, list[dict]] = {}
    for p in packages:
        by_name.setdefault(p["name"], []).append(p)

    def resolve(dep: str) -> dict | None:
        parts = dep.split(" ")
        cands = by_name.get(parts[0], [])
        if len(parts) >= 2:
            cands = [c for c in cands if c["version"] == parts[1]]
        return cands[0] if cands else None

    roots = by_name.get("sbcore", [])
    if not roots:
        fail("Cargo.lock has no `sbcore` package")
        return
    seen: set[tuple[str, str]] = set()
    stack = [(roots[0], ["sbcore"])]
    while stack:
        pkg, path = stack.pop()
        key = (pkg["name"], pkg["version"])
        if key in seen:
            continue
        seen.add(key)
        if PYO3_FAMILY.match(pkg["name"]):
            fail(f"sbcore transitively depends on {pkg['name']} via {' -> '.join(path)} (G1)")
            continue
        for dep in pkg.get("dependencies", []):
            child = resolve(dep)
            if child is not None:
                stack.append((child, [*path, child["name"]]))


def check_binding_manifest() -> None:
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
    deps = {real_name(n, s): s for _, n, s in manifest_deps(manifest)}
    pyo3 = deps.get("pyo3")
    if pyo3 is None:
        fail("Cargo.toml: the binding crate must depend on pyo3")
    elif isinstance(pyo3, dict):
        bad = {"experimental-async", "extension-module"} & set(pyo3.get("features", []))
        if bad:
            fail(f"Cargo.toml: pyo3 features {sorted(bad)} are forbidden (§2.3)")
    for name in sorted(DROPPED_CRATES & set(deps)):
        fail(f"Cargo.toml: {name!r} was deliberately dropped (§2.3)")
    lints = manifest.get("workspace", {}).get("lints", {}).get("clippy", {})
    level = lints.get("await_holding_lock")
    if (level if isinstance(level, str) else (level or {}).get("level")) not in ("deny", "forbid"):
        fail("Cargo.toml: [workspace.lints.clippy] await_holding_lock must be deny (I7)")


# ── source scans ─────────────────────────────────────────────────────────────────────────────────────


def check_no_attach_and_placeholders() -> None:
    for path in [*rust_files(BINDING_SRC), *rust_files(SBCORE / "src")]:
        src = strip_comments(path.read_text())
        for m in ATTACH.finditer(src):
            fail(f"{rel(path)}:{line_of(src, m.start())}: `{m.group(0)}` is forbidden (I2: no attach from Rust)")
        for m in re.finditer(r"\b(todo|unimplemented)\s*!\s*\(", src):
            fail(f"{rel(path)}:{line_of(src, m.start())}: `{m.group(1)}!()` is forbidden")


def check_binding_holds_no_python() -> None:
    item = re.compile(r"\b(struct|enum)\s+(\w+)")
    static = re.compile(r"\bstatic\s+(?:mut\s+)?(\w+)\s*:")
    for path in rust_files(BINDING_SRC):
        src = strip_comments(path.read_text())
        for word in ("__traverse__", "__clear__"):
            if word in src:
                fail(f"{rel(path)}: `{word}` is forbidden (§4.4: no native GC slots)")
        for m in item.finditer(src):
            # Find the body: the first `{` or `(` after the name, unless a `;` comes first (unit struct).
            j = m.end()
            while j < len(src) and src[j] not in "{(;":
                j += 1
            if j >= len(src) or src[j] == ";":
                continue
            body = src[j : matching(src, j)]
            hit = PY_HOLDING_TYPES.search(body)
            if hit:
                fail(
                    f"{rel(path)}:{line_of(src, m.start())}: {m.group(1)} {m.group(2)} holds a Python "
                    f"object (`{hit.group(0).strip()}`) (I1)"
                )
        for m in static.finditer(src):
            end = src.find("=", m.end())
            semi = src.find(";", m.end())
            stop = min(x for x in (end, semi) if x >= 0) if max(end, semi) >= 0 else len(src)
            ty = src[m.end() : stop]
            if PY_HOLDING_TYPES.search(ty) and not (path.name == "errors.rs" and m.group(1) == "EXC_TYPES"):
                fail(f"{rel(path)}:{line_of(src, m.start())}: static {m.group(1)} holds a Python object (I1)")


def check_pyclasses() -> None:
    gil_used = False
    for path in rust_files(BINDING_SRC):
        src = strip_comments(path.read_text())
        for m in re.finditer(r"#\[pyclass\b", src):
            attr = src[m.start() : matching(src, m.start() + 1)]
            if not re.search(r"\bfrozen\b", attr):
                fail(f"{rel(path)}:{line_of(src, m.start())}: every #[pyclass] must be `frozen` (§5.6)")
        if re.search(r"#\[pymodule\s*\(\s*gil_used\s*=\s*false\s*\)\s*\]", src):
            gil_used = True
    if not gil_used:
        fail("src/: the native module must be declared `#[pymodule(gil_used = false)]` (G7)")
    lib = strip_comments((BINDING_SRC / "lib.rs").read_text())
    if not re.search(
        r"#!\[(?:cfg_attr\(\s*not\(\s*test\s*\)\s*,\s*)?deny\([^\]]*clippy::unwrap_used[^\]]*clippy::expect_used", lib
    ):
        fail("src/lib.rs must deny clippy::unwrap_used and clippy::expect_used outside tests (§5.7)")


def check_no_public_guards() -> None:
    sig = re.compile(r"\bpub\s+fn\s+(\w+)\s*(?:<[^>]*>)?\s*\(")
    for path in rust_files(SBCORE / "src"):
        src = strip_comments(path.read_text())
        for m in sig.finditer(src):
            close = matching(src, m.end() - 1)
            brace = src.find("{", close)
            semi = src.find(";", close)
            stop = min(x for x in (brace, semi) if x >= 0) if max(brace, semi) >= 0 else len(src)
            ret = src[close:stop]
            if "->" in ret and LOCK_GUARD.search(ret):
                if path.name == "sync.rs" and m.group(1) == "lock_checked":
                    continue
                fail(f"{rel(path)}:{line_of(src, m.start())}: pub fn {m.group(1)} returns a lock guard (I7)")


def check_wake_seqcst() -> None:
    path = SBCORE / "src" / "wake.rs"
    src = strip_comments(path.read_text())
    found: set[tuple[str, str]] = set()
    for m in WAKE_OPS.finditer(src):
        call = src[m.start() : matching(src, m.end() - 1)]
        found.add((m.group(1), m.group(2)))
        if "SeqCst" not in call or NON_SEQCST.search(call):
            fail(f"{rel(path)}:{line_of(src, m.start())}: `{' '.join(call.split())}` must use SeqCst only (§5.4)")
    required = {("ready_word", "fetch_or"), ("pending", "swap"), ("pending", "store"), ("ready_word", "swap")}
    for name, op in sorted(required - found):
        fail(f"{rel(path)}: expected the WakeHub operation `{name}.{op}(…, SeqCst)` (§5.4)")


def main() -> int:
    check_sbcore_manifest()
    check_lockfile()
    check_binding_manifest()
    check_no_attach_and_placeholders()
    check_binding_holds_no_python()
    check_pyclasses()
    check_no_public_guards()
    check_wake_seqcst()
    if errors:
        print(f"check_invariants: {len(errors)} violation(s)", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        return 1
    print("check_invariants: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
