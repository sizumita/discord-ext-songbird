# Model-specific examples: `examples/lfm2_audio/` (2026-10-27)

`docs/design.md` (the 2026-10-27 decision, G6 and the non-goals) keeps the library and its examples
provider-neutral: no provider-specific APIs, presets, helpers, examples or tests, and every example
needs only `discord.py` (plus `websockets` for the bridge). At the user's request, this change adds
three example bots built on one specific model, Liquid AI's LFM2.5-Audio-1.5B-JP, run locally with
the `liquid-audio` package:

* `examples/lfm2_audio/stt.py`: speech-to-text (ASR mode).
* `examples/lfm2_audio/tts.py`: read-aloud bot (TTS mode, streamed frame by frame).
* `examples/lfm2_audio/voice_chat.py`: realtime voice chat (interleaved mode) with barge-in.

What stays as designed:

* **The library is unchanged.** No preset, helper, default or dependency was added to the package;
  the examples use only the public API (`AudioFormat(16_000, 1, "f32le")` and
  `AudioFormat(24_000, 1, "f32le")` are built in the examples, not exported presets).
* **The examples stay importable and tested in CI** (§9.10 rule 6, §13.3). `torch` and
  `liquid_audio` are imported inside the model wrappers only, so importing a module needs neither,
  and every bot function takes its model as an argument. `tests/test_examples.py` imports all three
  and runs their bot logic against a detached connection with fake models.
* **They live in their own directory** with their own README (setup, device notes), apart from the
  generic examples.

What differs:

* **Extra dependencies for running them** (`liquid-audio`, which pulls in PyTorch). They are not
  dev dependencies; `pyproject.toml` has a ty override that ignores `unresolved-import` under
  `examples/lfm2_audio/**`, so the rest of each file is still type-checked.
* `tests/test_config.py::test_documented_names_are_in_all` scans `examples/**/*.py` (it used to
  scan only the top level), so the new examples are covered too.
