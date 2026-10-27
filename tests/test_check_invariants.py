"""Self-test of ``scripts/check_invariants.py``: every rule rejects the patterns it exists to stop and
accepts the patterns the code base uses. CI's "Invariants" step is the only enforcement of G2/I1/I2
and of the §10 guards against the 0.5 defects 4 (broadcast ring) and 6 (user-sized eager allocation),
so a rule that silently stops matching must fail here."""

from __future__ import annotations

import importlib.util
import pathlib
import textwrap
from typing import Any

import pytest

ROOT = pathlib.Path(__file__).resolve().parent.parent


def _load() -> Any:
    spec = importlib.util.spec_from_file_location("check_invariants", ROOT / "scripts" / "check_invariants.py")
    assert spec is not None and spec.loader is not None
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


ci = _load()


def rust(src: str, *, label: str = "crates/sbcore/src/x.rs", production: bool = True) -> list[str]:
    ci.errors.clear()
    ci.scan_rust(label, textwrap.dedent(src), production=production)
    out = list(ci.errors)
    ci.errors.clear()
    return out


def binding(src: str, *, label: str = "src/py/x.rs") -> list[str]:
    ci.errors.clear()
    ci.scan_binding(label, textwrap.dedent(src))
    out = list(ci.errors)
    ci.errors.clear()
    return out


# ── G2 / I2: no attach from Rust ─────────────────────────────────────────────────────────────────────


@pytest.mark.parametrize(
    "call",
    [
        "Python::attach(|py| ())",
        "Python::try_attach(|py| ())",
        "pyo3::Python :: attach(|py| ())",
        "unsafe { Python::attach_unchecked(|py| ()) }",
        "unsafe { Python::assume_attached() }",
        "Python::with_gil(|py| ())",
        "unsafe { pyo3::ffi::PyGILState_Ensure() }",
        "unsafe { ffi::PyEval_RestoreThread(state) }",
        "unsafe { ffi::PyThreadState_Swap(state) }",
    ],
)
def test_attach_entry_points_are_rejected(call: str) -> None:
    errs = rust(f"fn on_mixer_thread() {{\n    {call};\n}}\n")
    assert len(errs) == 1 and ":2:" in errs[0] and "I2" in errs[0], errs


def test_attach_in_comments_and_literals_is_ignored() -> None:
    src = """\
        // Python::attach would deadlock here
        /* Python::with_gil */
        const DOC: &str = "never Python::attach";
        const RAW: &str = r#"a "quoted" Python::attach_unchecked"#;
        fn f(py: Python<'_>) { py.detach(|| ()); }
    """
    assert rust(src) == []


# ── I1: no Python object in binding state ────────────────────────────────────────────────────────────


@pytest.mark.parametrize(
    "field",
    [
        "last_error: Mutex<Option<PyErr>>",
        "result: Option<PyResult<()>>",
        "name: PyBackedStr",
        "data: PyBackedBytes",
        "init: Option<PyClassInitializer<Track>>",
        "obj: Py<PyAny>",
        "cb: Mutex<Option<Bound<'static, PyAny>>>",
        "this: PyRef<'static, Connection>",
    ],
)
def test_python_holding_fields_are_rejected(field: str) -> None:
    errs = binding(f"#[pyclass(frozen)]\nstruct Holder {{\n    {field},\n}}\n")
    assert len(errs) == 1 and "struct Holder holds a Python object" in errs[0], errs


def test_python_holding_enum_variant_and_static_are_rejected() -> None:
    errs = binding("enum Outcome { Done(u32), Failed(PyErr) }\nstatic LAST: Mutex<Option<PyErr>> = Mutex::new(None);\n")
    assert len(errs) == 2, errs
    assert "enum Outcome" in errs[0] and "static LAST" in errs[1]


def test_binding_state_without_python_objects_passes() -> None:
    src = """\
        #[pyclass(frozen)]
        pub struct Connection {
            shared: Arc<sbcore::conn::Shared>,
            #[doc = "PyErr is only a return type"]
            id: u64,
        }
        fn take(&self, py: Python<'_>) -> PyResult<Vec<(u32, Py<PyAny>)>> { todo_free() }
    """
    assert binding(src) == []


def test_exc_types_allowance_is_limited_to_errors_rs() -> None:
    src = "static EXC_TYPES: [PyOnceLock<Py<PyType>>; 3] = [PyOnceLock::new(), PyOnceLock::new(), PyOnceLock::new()];\n"
    assert binding(src, label="src/errors.rs") == []
    assert len(binding(src, label="src/py/other.rs")) == 1


# ── §10, 0.5 defect 4: no broadcast ──────────────────────────────────────────────────────────────────


@pytest.mark.parametrize(
    "src",
    [
        "fn f(n: usize) { let (tx, _rx) = tokio::sync::broadcast::channel::<u8>(n * 50); }",
        "use tokio::sync::{broadcast, mpsc};",
        "fn f() { let (s, r) = async_broadcast::broadcast::<u8>(16); }",
    ],
)
def test_broadcast_is_rejected(src: str) -> None:
    errs = rust(src)
    assert errs and all("defect 4" in e for e in errs), errs


def test_broadcast_is_rejected_in_test_code_too() -> None:
    src = "#[cfg(test)]\nmod tests {\n    use tokio::sync::broadcast;\n}\n"
    assert len(rust(src)) == 1
    assert len(rust(src, label="crates/sbcore/src/x/tests.rs", production=False)) == 1


def test_broadcast_lookalikes_pass() -> None:
    assert rust('fn rebroadcast_speaking() { log("broadcast"); } // broadcast\n') == []


# ── §10, 0.5 defect 6: no user-sized with_capacity ───────────────────────────────────────────────────


@pytest.mark.parametrize(
    "arg",
    [
        "max_queue_ms / chunk_ms",
        "secs * 50",
        "n",
        "cfg.max_packets",
        "data.len() * 6",
        "cap.min(16) * frames",
        "(n).max(16)",
    ],
)
def test_user_sized_capacity_is_rejected(arg: str) -> None:
    errs = rust(f"fn f() {{\n    let q = VecDeque::<u8>::with_capacity({arg});\n}}\n")
    assert len(errs) == 1 and ":2:" in errs[0] and "defect 6" in errs[0], errs


def test_with_capacity_and_hasher_is_checked() -> None:
    assert len(rust("fn f(n: usize) { HashMap::with_capacity_and_hasher(n, S::default()); }")) == 1
    assert rust("fn f() { HashMap::with_capacity_and_hasher(16, S::default()); }") == []


@pytest.mark.parametrize(
    "arg",
    [
        "16",
        "4_096usize",
        "1 << 10",
        "GRAVE_CAP",
        "Self::CAP * 2",
        "limits::MAX_TRACKS + 1",
        "(FRAMES * CHANNELS)",
        "cap.min(16)",
        "max_chunks.min(INITIAL_CAPACITY)",
        "(bound / 2).min(SEGS_INITIAL)",
        "done.len()",
        "self.items.len()",
    ],
)
def test_bounded_capacity_passes(arg: str) -> None:
    assert rust(f"fn f() {{ let v = Vec::<u8>::with_capacity({arg}); }}") == []


def test_capacity_in_test_only_code_is_not_checked() -> None:
    src = """\
        #[cfg(test)]
        mod tests {
            fn t(n: usize) { let _ = Vec::<u8>::with_capacity(n * 4); let s = "}"; let c = '}'; }
        }
        #[cfg(all(test, unix))]
        fn helper(n: usize) -> Vec<u8> { Vec::with_capacity(n) }
        fn production(n: usize) -> Vec<u8> { Vec::with_capacity(n) }
    """
    errs = rust(src)
    assert len(errs) == 1 and ":7:" in errs[0], errs
    assert rust(src, production=False) == []


def test_capacity_allow_list_is_per_file() -> None:
    src = "fn f(cap: usize) -> Vec<f32> { Vec::with_capacity(cap) }"
    assert rust(src, label="crates/sbcore/src/dsp/resample.rs") == []
    assert len(rust(src, label="crates/sbcore/src/dsp/mix.rs")) == 1


def test_reintroducing_the_05_patterns_into_rx_queue_fails() -> None:
    """The finding's scenario: both 0.5 allocation patterns appended to the real ``rx/queue.rs``."""
    text = (ROOT / "crates" / "sbcore" / "src" / "rx" / "queue.rs").read_text()
    assert rust(text, label="crates/sbcore/src/rx/queue.rs") == []
    text += textwrap.dedent(
        """
        fn regress(user_secs: usize, max_queue_ms: usize, chunk_ms: usize) {
            let (_tx, _rx) = tokio::sync::broadcast::channel::<u8>(user_secs * 50);
            let _q: VecDeque<u8> = VecDeque::with_capacity(max_queue_ms / chunk_ms);
        }
        """
    )
    errs = rust(text, label="crates/sbcore/src/rx/queue.rs")
    assert len(errs) == 2 and "defect 4" in errs[0] and "defect 6" in errs[1], errs


# ── helpers ──────────────────────────────────────────────────────────────────────────────────────────


def test_preprocessing_keeps_line_numbers() -> None:
    src = 'let a = r#"x\n"y"\n"#;\n/* a\nb */ let b = \'\\\'\';\n// c\nlet s = "q\\"\n";\n'
    code = ci.code_of(src)
    assert code.count("\n") == src.count("\n")
    assert ci.strip_cfg_test("#[cfg(test)]\nmod t {\n}\nfn f() {}\n").count("\n") == 4


@pytest.mark.parametrize(
    ("path", "test_only"),
    [
        ("crates/sbcore/src/tx/queue/tests.rs", True),
        ("crates/sbcore/src/rx/pipeline_tests.rs", True),
        ("crates/sbcore/src/rx/test_util.rs", True),
        ("crates/sbcore/src/conn/shared/tests/race.rs", True),
        ("crates/sbcore/src/testing/rtp.rs", True),
        ("src/py/testing.rs", True),
        ("crates/sbcore/src/rx/queue.rs", False),
        ("src/py/connection.rs", False),
    ],
)
def test_is_test_only(path: str, test_only: bool) -> None:
    assert ci.is_test_only(ROOT / path) is test_only


# ── §4.10: entry points check for a forked child before touching state ─────────────────────────────


def fork_guards(src: str, *, label: str = "src/py/x.rs") -> list[str]:
    ci.errors.clear()
    ci.scan_fork_guards(label, textwrap.dedent(src))
    out = list(ci.errors)
    ci.errors.clear()
    return out


def test_unguarded_handle_method_is_rejected() -> None:
    """PR #12 review: ``OpusSink.clear()`` took the queue lock in a forked child."""
    errs = fork_guards(
        """
        #[pymethods]
        impl OpusSink {
            fn clear(&self) -> u64 {
                self.q.clear()
            }
        }
        """
    )
    assert len(errs) == 1 and "OpusSink.clear" in errs[0] and "§4.10" in errs[0], errs


def test_guard_after_the_first_state_access_is_rejected() -> None:
    errs = fork_guards(
        """
        #[pymethods]
        impl Playback {
            fn stats(&self, py: Python<'_>) -> PyResult<PlaybackStats> {
                let s = self.q.stats();
                ensure_not_forked(py)?;
                Ok(PlaybackStats { s })
            }
        }
        """
    )
    assert len(errs) == 1 and "Playback.stats" in errs[0], errs


def test_guard_in_a_comment_or_string_does_not_count() -> None:
    errs = fork_guards(
        """
        #[pymethods]
        impl OpusSink {
            // ensure_not_forked(py)?;
            fn clear(&self) -> u64 {
                let _ = "ensure_not_forked(py)";
                self.q.clear()
            }
        }
        """
    )
    assert len(errs) == 1, errs


def test_unguarded_pyfunction_is_rejected() -> None:
    errs = fork_guards(
        """
        #[gen_stub_pyfunction]
        #[pyfunction]
        pub fn live_counts() -> u64 {
            runtime::count()
        }
        """
    )
    assert len(errs) == 1 and "live_counts" in errs[0], errs


def test_guarded_methods_and_no_op_closes_pass() -> None:
    assert not fork_guards(
        """
        #[pymethods]
        impl OpusSink {
            fn clear(&self, py: Python<'_>) -> PyResult<u64> {
                ensure_not_forked(py)?;
                Ok(self.q.clear())
            }
            #[pyo3(signature = (drain = false))]
            fn close(&self, drain: bool) {
                if !forked() {
                    LiveSource::close(&*self.q, close_mode(drain));
                }
            }
            fn stop(&self) {
                if sbcore::runtime::check_not_forked().is_ok() {
                    self.q.stop();
                }
            }
        }
        #[pyfunction]
        pub fn log_wait(py: Python<'_>) -> Vec<u8> {
            if sbcore::runtime::FORKED.load(Ordering::Acquire) {
                return Vec::new();
            }
            drain()
        }
        """
    )


def test_value_classes_are_exempt_except_their_constructors() -> None:
    getter = """
        #[pymethods]
        impl AudioChunk {
            #[getter]
            fn seq(&self) -> u64 {
                self.seq
            }
        }
        """
    assert not fork_guards(getter)
    errs = fork_guards(
        """
        #[pymethods]
        impl RawPcm {
            #[new]
            fn py_new(data: &Bound<'_, PyAny>) -> PyResult<Self> {
                Ok(RawPcm { p: parse(data)? })
            }
        }
        """
    )
    assert len(errs) == 1 and "RawPcm.py_new" in errs[0], errs


def test_fork_exemptions_are_per_class() -> None:
    src = """
        #[pymethods]
        impl {cls} {{
            fn __hash__(&self) -> u64 {{
                self.h
            }}
        }}
        """
    assert not fork_guards(src.format(cls="NativeTrack"))
    assert len(fork_guards(src.format(cls="Playback"))) == 1
