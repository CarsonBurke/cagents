# Dictation fixtures

`en.wav` is `test_wavs/en.wav` from
[csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8](https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/tree/ed5d745623472f97cfc2e6c2916ee248798fc5a8/test_wavs),
a repository published under [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/)
(the Parakeet weights are NVIDIA's; the sherpa-onnx export is by the k2-fsa
project). It is
3.8 seconds of English speech, 24 kHz mono, so it also exercises resampling to
the models' 16 kHz.

Play it into a dictation session in place of a microphone with
`CAGENTS_DICTATION_INPUT=wav:/absolute/path/to/en.wav`, or transcribe it with
`cagents-dictate transcribe fixtures/dictation/en.wav`.
