//! The speech models the helper can run.
//!
//! Every file is pinned: a Hugging Face revision or a release asset, its size
//! and its SHA-256. A model is downloaded only when someone asks for it, never
//! bundled, and nothing unverified is ever loaded.

/// One file of a model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelFile {
    /// The name it is stored under in the model's directory.
    pub name: &'static str,
    pub url: &'static str,
    pub size: u64,
    /// Lowercase hex.
    pub sha256: &'static str,
}

/// What the files of a model are, for the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Architecture {
    /// A NeMo transducer (Parakeet TDT): `encoder`, `decoder` and `joiner`
    /// ONNX graphs with a `tokens` table, transcribing one utterance at a
    /// time, with `vad` (Silero) finding the utterances.
    OfflineTransducer {
        encoder: &'static str,
        decoder: &'static str,
        joiner: &'static str,
        tokens: &'static str,
        vad: &'static str,
    },
}

/// A model in the catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Model {
    /// Stable identifier, used in settings and the protocol.
    pub id: &'static str,
    /// The pinned upstream revision. A model directory is named after the
    /// id and this, so a newer catalog never writes into files that an
    /// older build still loads.
    pub revision: &'static str,
    pub name: &'static str,
    /// Which languages it transcribes, for a person choosing.
    pub languages: &'static str,
    /// Who made it and under what licence, which the licence requires us to
    /// show.
    pub attribution: &'static str,
    pub architecture: Architecture,
    pub files: &'static [ModelFile],
}

impl Model {
    /// Bytes to download for the whole model.
    pub fn size(&self) -> u64 {
        self.files.iter().map(|file| file.size).sum()
    }

    /// The directory name the model is stored under.
    pub fn directory_name(&self) -> String {
        format!("{}@{}", self.id, &self.revision[..12])
    }
}

/// The model a new installation dictates with.
///
/// Parakeet TDT 0.6B v3 transcribes 25 European languages and detects which
/// is spoken. v2 is a little more accurate in English only; a wrong-language
/// default fails outright, while the English-only gain is small, so v3 is
/// the default and v2 is a choice in Settings.
pub const DEFAULT: &str = PARAKEET_V3.id;

/// Every model, in the order Settings lists them.
pub const MODELS: &[Model] = &[PARAKEET_V3, PARAKEET_V2];

pub fn find(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|model| model.id == id)
}

const PARAKEET_ARCHITECTURE: Architecture = Architecture::OfflineTransducer {
    encoder: "encoder.int8.onnx",
    decoder: "decoder.int8.onnx",
    joiner: "joiner.int8.onnx",
    tokens: "tokens.txt",
    vad: "silero_vad.onnx",
};

/// Silero VAD, MIT, as sherpa-onnx publishes it.
const SILERO_VAD: ModelFile = ModelFile {
    name: "silero_vad.onnx",
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
    size: 643_854,
    sha256: "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6",
};

const PARAKEET_ATTRIBUTION: &str = "NVIDIA Parakeet TDT, CC BY 4.0, converted by sherpa-onnx";

macro_rules! hugging_face {
    ($repository:literal, $revision:literal, $name:literal, $size:literal, $sha256:literal) => {
        ModelFile {
            name: $name,
            url: concat!(
                "https://huggingface.co/",
                $repository,
                "/resolve/",
                $revision,
                "/",
                $name
            ),
            size: $size,
            sha256: $sha256,
        }
    };
}

const PARAKEET_V3: Model = Model {
    id: "parakeet-tdt-0.6b-v3",
    revision: "ed5d745623472f97cfc2e6c2916ee248798fc5a8",
    name: "Parakeet TDT 0.6B v3",
    languages: "25 European languages",
    attribution: PARAKEET_ATTRIBUTION,
    architecture: PARAKEET_ARCHITECTURE,
    files: &[
        hugging_face!(
            "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
            "ed5d745623472f97cfc2e6c2916ee248798fc5a8",
            "encoder.int8.onnx",
            652_282_294,
            "1afbe69c1ffdabee94755f42e4daf1d93549131a3da11356bda84fcef0f2c479"
        ),
        hugging_face!(
            "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
            "ed5d745623472f97cfc2e6c2916ee248798fc5a8",
            "decoder.int8.onnx",
            11_845_274,
            "0222a86147299d4dee90edf95b6d13827edb42ad8b7c9cf0b2582f030937c0b5"
        ),
        hugging_face!(
            "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
            "ed5d745623472f97cfc2e6c2916ee248798fc5a8",
            "joiner.int8.onnx",
            6_355_277,
            "cbd02f3696e9944970111fb439d9c3fd365a80374565f13f2b9c7007d253fc98"
        ),
        hugging_face!(
            "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
            "ed5d745623472f97cfc2e6c2916ee248798fc5a8",
            "tokens.txt",
            93_939,
            "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d"
        ),
        SILERO_VAD,
    ],
};

const PARAKEET_V2: Model = Model {
    id: "parakeet-tdt-0.6b-v2",
    revision: "84b478d541cdbf0c296a7ab5f1e20a6f8f998114",
    name: "Parakeet TDT 0.6B v2",
    languages: "English",
    attribution: PARAKEET_ATTRIBUTION,
    architecture: PARAKEET_ARCHITECTURE,
    files: &[
        hugging_face!(
            "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8",
            "84b478d541cdbf0c296a7ab5f1e20a6f8f998114",
            "encoder.int8.onnx",
            652_282_309,
            "92a10bd9c23936ea0566c29f963bc57c2a5df4261b6a8921c39abea66557ca8b"
        ),
        hugging_face!(
            "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8",
            "84b478d541cdbf0c296a7ab5f1e20a6f8f998114",
            "decoder.int8.onnx",
            7_257_753,
            "4d097bae3bac5c96018d5293ba39f1b657296102b142affda40378f38b020f41"
        ),
        hugging_face!(
            "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8",
            "84b478d541cdbf0c296a7ab5f1e20a6f8f998114",
            "joiner.int8.onnx",
            1_739_080,
            "77a29f3e5ac7d37074e0af465391b80f314015d5b97f0f90c6c7767f1d56f428"
        ),
        hugging_face!(
            "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8",
            "84b478d541cdbf0c296a7ab5f1e20a6f8f998114",
            "tokens.txt",
            9_384,
            "ec182b70dd42113aff6c5372c75cac58c952443eb22322f57bbd7f53977d497d"
        ),
        SILERO_VAD,
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_model_is_fully_pinned() {
        for model in MODELS {
            assert_eq!(model.revision.len(), 40, "{}", model.id);
            let Architecture::OfflineTransducer {
                encoder,
                decoder,
                joiner,
                tokens,
                vad,
            } = model.architecture;
            for name in [encoder, decoder, joiner, tokens, vad] {
                assert!(
                    model.files.iter().any(|file| file.name == name),
                    "{} has no {name}",
                    model.id
                );
            }
            for file in model.files {
                assert_eq!(file.sha256.len(), 64, "{}", file.name);
                assert!(file.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
                assert!(file.url.starts_with("https://"));
                assert!(file.size > 0);
                assert!(!file.name.contains('/') && !file.name.starts_with('.'));
                if file.url.starts_with("https://huggingface.co/") {
                    assert!(
                        file.url.contains(model.revision),
                        "{} is not pinned to {}",
                        file.url,
                        model.revision
                    );
                }
            }
        }
        assert!(find(DEFAULT).is_some());
    }

    #[test]
    fn directories_name_the_revision() {
        assert_eq!(
            find("parakeet-tdt-0.6b-v3").unwrap().directory_name(),
            "parakeet-tdt-0.6b-v3@ed5d74562347"
        );
    }
}
