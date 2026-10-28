# LFM2.5-Audio-1.5B-JP examples

Three bots that run Liquid AI's [LFM2.5-Audio-1.5B-JP](https://huggingface.co/LiquidAI/LFM2.5-Audio-1.5B-JP),
a Japanese speech-to-speech model, locally with the [`liquid-audio`](https://github.com/Liquid4All/liquid-audio)
package. Each file is self-contained.

| File | Command | What it does | Model mode |
|---|---|---|---|
| [`stt.py`](stt.py) | `!transcribe` (`!stop`) | Posts what each member of the voice channel says, one message per utterance | ASR (`generate_sequential`, prompt `Perform ASR in japanese.`) |
| [`tts.py`](tts.py) | `!tts` (`!skip`, `!stop`) | Reads every new message of the text channel aloud | TTS (`generate_sequential`, prompt `Perform TTS in japanese.`) |
| [`voice_chat.py`](voice_chat.py) | `!chat` (`!stop`) | Realtime spoken conversation: speak, pause, hear the answer; talk over it to interrupt | Interleaved (`generate_interleaved`, prompt `Respond with interleaved text and audio.`) |

The prompts are the fixed strings the model card documents for each mode.

## Setup

```bash
uv sync --all-extras --dev --no-install-project
uv run --no-sync maturin develop
uv pip install liquid-audio
```

- `liquid-audio` pulls in PyTorch, torchaudio and transformers, so the virtualenv's Python (3.14, or
  3.14t when uv picks the free-threaded build) needs PyTorch wheels. `pip install flash-attn` is
  optional (without it, attention uses PyTorch's SDPA).
- The model (about 3.7 GB) is downloaded from Hugging Face on first start.
- **Device.** An NVIDIA GPU (CUDA) is the main target: streamed speech only plays smoothly while
  generation is faster than real time. On CUDA, also `pip install causal-conv1d` (without it,
  transformers warns and uses a slower reference kernel). The examples decode audio with the streaming
  Mimi decoder rather than `processor.decode()`, whose LFM-based detokenizer only runs on CUDA, so they
  also run on Apple Silicon (`mps`) and CPU. On an Apple M4 (`mps`), the first audio of an answer came
  0.4 s after the prompt, but generation ran at about 0.7–0.9× real time, so playback stutters. There,
  raise `PREBUFFER_MS` (up to half of `max_buffer_ms`) to trade a later start for smooth audio.
  `LFM_DEVICE` (`cuda`, `mps`, `cpu`) overrides the automatic choice.
- Enable the **Message Content** intent for the bot in the Discord developer portal.

```bash
export DISCORD_BOT_TOKEN=...
python examples/lfm2_audio/voice_chat.py
```

Environment: `DISCORD_BOT_TOKEN`; optionally `LFM_MODEL` (default `LiquidAI/LFM2.5-Audio-1.5B-JP`)
and `LFM_DEVICE`. Check the model's license (LFM Open License v1.0) before using it.

## How they work

- **Formats.** Audio is received in 16 kHz mono float32, `AudioFormat(16_000, 1, "f32le")`: the rate of the
  model's audio encoder, so the receive pipeline does all the conversion. Generated audio is played in the
  decoder's own format, 24 kHz mono float32; the mixer converts it to Discord's.
- **Threading.** All inference runs on one dedicated thread (`INFERENCE`), never on the event loop and never
  two jobs at once. `iterate_on_worker` hands the frames of a running generation to the event loop through
  a bounded hand-off, and `await speaker.write()` waits while the playback buffer is full, so the model
  never runs further ahead than the buffer. Closing the iterator stops the generation after its current
  token.
- **Streaming output.** Mimi turns every 80 ms audio frame into audio as soon as the model emits it, so
  speech starts playing after the first few tokens.
- **Turn-taking** uses `chunk.speaking`: an utterance starts at the first voiced chunk and ends after
  `END_SILENCE_MS` without speech. `MIN_SPEECH_MS` filters out blips and `MAX_UTTERANCE_MS` caps
  memory use.
- **Barge-in** (`voice_chat.py`). Each answer is written under its own tag. When someone speaks
  `BARGE_IN_MS` over it, `clear()` silences it within one or two ticks and the generation is stopped.
  The tag's `heard_ms` in the `ClearResult` tells how much was heard, and only that part of the answer
  stays in the conversation history.
- **History** (`voice_chat.py`). The last `HISTORY_TURNS` exchanges are kept as model tokens and
  rebuilt into the prompt for every answer. The model prefills the whole context each time, so a
  longer history means slower answers.

The tunables are module constants at the top of each file. `tests/test_examples.py` runs the bot logic
of all three against a detached voice connection, with fake models in place of LFM2.5-Audio.

## Notes

- For the best quality on CUDA, decode a complete utterance with `processor.decode(codes)` (the LFM-based
  detokenizer) instead of streaming Mimi frames; playback then starts only once the utterance is fully generated.
- The model is small. In testing, spoken follow-ups kept the context of earlier turns, but answers were
  often factually wrong, and after a self-introduction it tended to introduce itself again.
- Each bot loads its own copy of the model. To run several modes in one process, share one processor and
  model (and the one inference thread) between them.
- Unlike the rest of the repository, these examples are tied to one model. They are not part of the
  library and add no dependency to it (see `docs/deviations/lfm2-audio-examples.md`).
