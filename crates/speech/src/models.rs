//! The speech model catalogue and the model store.
//!
//! Models are downloaded on first use, after the user confirms (the dialog shows the size, the
//! source URL and the licence), into `<data dir>/models/<id>/`. They are never bundled with
//! FilmCraft or committed to the repository. Every file is pinned to a Hugging Face revision of
//! the publisher's own repository and checked against its SHA-256 before it is used.
//!
//! - **Parakeet** (speech recognition, the default) and **TitaNet** (speaker embeddings): NVIDIA,
//!   CC-BY-4.0. NVIDIA publishes them as `.nemo` files (uncompressed tar archives); only the
//!   members FilmCraft reads are downloaded, as byte ranges of NVIDIA's file ([`ModelFile::offset`]).
//! - **Whisper**: OpenAI released the code and model weights under the MIT licence
//!   (<https://github.com/openai/whisper/blob/main/LICENSE>); the safetensors conversions OpenAI
//!   publishes at `huggingface.co/openai/whisper-*` are labelled Apache-2.0.

use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::Digest;

/// One file of a model.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ModelFile {
    pub name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
    /// The file is `size` bytes of `url` starting here (a member of a `.nemo` archive), fetched
    /// with an HTTP range request; `None` = the whole of `url`.
    pub offset: Option<u64>,
}

/// What a model does and which runtime runs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    /// OpenAI Whisper speech recognition (feature `whisper`).
    Whisper,
    /// NVIDIA Parakeet TDT speech recognition (feature `parakeet`).
    Parakeet,
    /// Speaker embeddings for speaker labels (feature `parakeet`).
    Speaker,
}

impl Engine {
    /// The cargo feature that builds this engine.
    pub const fn feature(self) -> &'static str {
        match self {
            Engine::Whisper => "whisper",
            Engine::Parakeet | Engine::Speaker => "parakeet",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "whisper" => Some(Engine::Whisper),
            "parakeet" => Some(Engine::Parakeet),
            _ => None,
        }
    }
}

/// A downloadable speech model.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ModelInfo {
    pub id: &'static str,
    pub engine: Engine,
    pub name: &'static str,
    /// ISO 639-1 codes it transcribes; empty = any (Whisper detects the language).
    pub languages: &'static [&'static str],
    /// Recognises languages other than English (and detects the language).
    pub multilingual: bool,
    pub description: &'static str,
    pub license: &'static str,
    pub license_url: &'static str,
    pub author: &'static str,
    /// Human-readable source page.
    pub source: &'static str,
    pub files: &'static [ModelFile],
}

impl ModelInfo {
    pub fn size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

const LICENSE: &str = "MIT (OpenAI Whisper weights); Hugging Face conversion: Apache-2.0";
const LICENSE_URL: &str = "https://github.com/openai/whisper/blob/main/LICENSE";
const AUTHOR: &str = "OpenAI";

macro_rules! hf {
    ($repo:literal, $rev:literal, $name:literal, $sha:literal, $size:expr) => {
        ModelFile { name: $name, url: concat!("https://huggingface.co/openai/", $repo, "/resolve/", $rev, "/", $name), sha256: $sha, size: $size, offset: None }
    };
}

const TOKENIZER_MULTI: &str = "27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566";

/// A member of an NVIDIA `.nemo` archive on Hugging Face.
macro_rules! nemo {
    ($repo:literal, $rev:literal, $file:literal, $name:literal, $offset:expr, $size:expr, $sha:literal) => {
        ModelFile {
            name: $name,
            url: concat!("https://huggingface.co/nvidia/", $repo, "/resolve/", $rev, "/", $file),
            sha256: $sha,
            size: $size,
            offset: Some($offset),
        }
    };
}

const NVIDIA_LICENSE: &str = "CC-BY-4.0 (NVIDIA)";
const NVIDIA_LICENSE_URL: &str = "https://creativecommons.org/licenses/by/4.0/";

/// The 25 languages of Parakeet TDT 0.6B v3.
const EUROPEAN: &[&str] =
    &["bg", "hr", "cs", "da", "nl", "en", "et", "fi", "fr", "de", "el", "hu", "it", "lv", "lt", "mt", "pl", "pt", "ro", "sk", "sl", "es", "sv", "ru", "uk"];

static CATALOGUE: &[ModelInfo] = &[
    ModelInfo {
        id: "parakeet-tdt-0.6b-v2",
        engine: Engine::Parakeet,
        name: "Parakeet TDT 0.6B v2 (English)",
        languages: &["en"],
        multilingual: false,
        description: "600 M parameters, English with punctuation and capitals. The default: the most accurate English model here and fast on the GPU.",
        license: NVIDIA_LICENSE,
        license_url: NVIDIA_LICENSE_URL,
        author: "NVIDIA",
        source: "https://huggingface.co/nvidia/parakeet-tdt-0.6b-v2",
        files: &[
            nemo!(
                "parakeet-tdt-0.6b-v2",
                "c00e38779b028d48624b08ab3d9909952db43901",
                "parakeet-tdt-0.6b-v2.nemo",
                "tokenizer.vocab",
                263_168,
                10_393,
                "dbe7ec6bd066c4526080940e1614e6582b9161515e856c2c39a1d49267c88cbc"
            ),
            nemo!(
                "parakeet-tdt-0.6b-v2",
                "c00e38779b028d48624b08ab3d9909952db43901",
                "parakeet-tdt-0.6b-v2.nemo",
                "model_weights.ckpt",
                298_496,
                2_471_920_514,
                "fb1cbe2765fb80c02c0f874b6ea2c2bd24048c3618c092584fd086c6366e48a9"
            ),
        ],
    },
    ModelInfo {
        id: "parakeet-tdt-0.6b-v3",
        engine: Engine::Parakeet,
        name: "Parakeet TDT 0.6B v3 (25 European languages)",
        languages: EUROPEAN,
        multilingual: true,
        description: "600 M parameters, 25 European languages with punctuation and capitals.",
        license: NVIDIA_LICENSE,
        license_url: NVIDIA_LICENSE_URL,
        author: "NVIDIA",
        source: "https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3",
        files: &[
            nemo!(
                "parakeet-tdt-0.6b-v3",
                "541d1f99c6b0c3cd0b11a95167540bb8edefd82b",
                "parakeet-tdt-0.6b-v3.nemo",
                "tokenizer.vocab",
                3_072,
                101_024,
                "41130ff456706304a1adec782ccc9e003c4d417e8e324353d281be958cac4e17"
            ),
            nemo!(
                "parakeet-tdt-0.6b-v3",
                "541d1f99c6b0c3cd0b11a95167540bb8edefd82b",
                "parakeet-tdt-0.6b-v3.nemo",
                "model_weights.ckpt",
                680_448,
                2_508_649_346,
                "381d1cdc01457395bf9ca4f64a91bbe6d042b6fccd4d9aa88a52d15d63966d99"
            ),
        ],
    },
    ModelInfo {
        id: "titanet-large",
        engine: Engine::Speaker,
        name: "TitaNet-large speaker embeddings",
        languages: &[],
        multilingual: true,
        description: "23 M parameters. Tells voices apart for speaker labels (any language).",
        license: NVIDIA_LICENSE,
        license_url: NVIDIA_LICENSE_URL,
        author: "NVIDIA",
        source: "https://huggingface.co/nvidia/speakerverification_en_titanet_large",
        files: &[nemo!(
            "speakerverification_en_titanet_large",
            "0dc382f40121a5fbd34db10a2bb04d826c2be6a8",
            "speakerverification_en_titanet_large.nemo",
            "model_weights.ckpt",
            4_096,
            101_611_209,
            "7b17bd79997340b54032d386347727a6e9bc67f3a91041587175245156b86932"
        )],
    },
    ModelInfo {
        id: "whisper-tiny",
        engine: Engine::Whisper,
        languages: &[],
        name: "Whisper tiny (multilingual)",
        multilingual: true,
        description: "39 M parameters. Fastest; fine for clear speech, rough on noisy audio.",
        license: LICENSE,
        license_url: LICENSE_URL,
        author: AUTHOR,
        source: "https://huggingface.co/openai/whisper-tiny",
        files: &[
            hf!(
                "whisper-tiny",
                "169d4a4341b33bc18d8881c4b69c2e104e1cc0af",
                "config.json",
                "ffdccec4f3211f4c63310f2b7098f309fe70f3952cedc5e4d11e43f5b2379b98",
                1983
            ),
            hf!(
                "whisper-tiny",
                "169d4a4341b33bc18d8881c4b69c2e104e1cc0af",
                "generation_config.json",
                "a5d5325911f16e74001a72fa13d6e208eee51548f994646de1f4b4cc8b35b512",
                3747
            ),
            hf!(
                "whisper-tiny",
                "169d4a4341b33bc18d8881c4b69c2e104e1cc0af",
                "tokenizer.json",
                "27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566",
                2480466
            ),
            hf!(
                "whisper-tiny",
                "169d4a4341b33bc18d8881c4b69c2e104e1cc0af",
                "model.safetensors",
                "7ebd0e69e78190ffe1438491fa05cc1f5c1aa3a4c4db3bc1723adbb551ea2395",
                151061672
            ),
        ],
    },
    ModelInfo {
        id: "whisper-base",
        engine: Engine::Whisper,
        languages: &[],
        name: "Whisper base (multilingual)",
        multilingual: true,
        description: "74 M parameters. The default: a good balance of speed and accuracy.",
        license: LICENSE,
        license_url: LICENSE_URL,
        author: AUTHOR,
        source: "https://huggingface.co/openai/whisper-base",
        files: &[
            hf!(
                "whisper-base",
                "e37978b90ca9030d5170a5c07aadb050351a65bb",
                "config.json",
                "a153c53883a6799b6f056b4a8d1a515c9926d03994682ba88a7616618d7da0c1",
                1983
            ),
            hf!(
                "whisper-base",
                "e37978b90ca9030d5170a5c07aadb050351a65bb",
                "generation_config.json",
                "444b3f636d2fff89dd9ecf549e2a085b61f7ff0fa0246d4628bac6a3b8cc9ba4",
                3807
            ),
            hf!(
                "whisper-base",
                "e37978b90ca9030d5170a5c07aadb050351a65bb",
                "tokenizer.json",
                "27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566",
                2480466
            ),
            hf!(
                "whisper-base",
                "e37978b90ca9030d5170a5c07aadb050351a65bb",
                "model.safetensors",
                "07cadb9f25677c8d50df603e66a98fbd842cce45047139baeb16e6219a1e807b",
                290403936
            ),
        ],
    },
    ModelInfo {
        id: "whisper-small",
        engine: Engine::Whisper,
        languages: &[],
        name: "Whisper small (multilingual)",
        multilingual: true,
        description: "244 M parameters. Most accurate here; about 4× slower than base.",
        license: LICENSE,
        license_url: LICENSE_URL,
        author: AUTHOR,
        source: "https://huggingface.co/openai/whisper-small",
        files: &[
            hf!(
                "whisper-small",
                "973afd24965f72e36ca33b3055d56a652f456b4d",
                "config.json",
                "e6a2b489da1b5aed65a8eb8d1e7466fa867ad5643a8bc138ba708bd56b2875c4",
                1967
            ),
            hf!(
                "whisper-small",
                "973afd24965f72e36ca33b3055d56a652f456b4d",
                "generation_config.json",
                "71565b8ef50d0bf7a1193ed4bbed195b94e70c18894d81bba2f1233dcec3ab53",
                3868
            ),
            hf!(
                "whisper-small",
                "973afd24965f72e36ca33b3055d56a652f456b4d",
                "tokenizer.json",
                "27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566",
                2480466
            ),
            hf!(
                "whisper-small",
                "973afd24965f72e36ca33b3055d56a652f456b4d",
                "model.safetensors",
                "1d7734884874f1a1513ed9aa760a4f8e97aaa02fd6d93a3a85d27b2ae9ca596b",
                966995080
            ),
        ],
    },
];

/// The default speech recogniser.
pub const DEFAULT_MODEL: &str = "parakeet-tdt-0.6b-v2";
/// The default Whisper model (the fallback for languages Parakeet does not transcribe).
pub const DEFAULT_WHISPER: &str = "whisper-base";
/// The speaker embedding model.
pub const SPEAKER_MODEL: &str = "titanet-large";

/// The default model of `engine`.
pub const fn default_for(engine: Engine) -> &'static str {
    match engine {
        Engine::Whisper => DEFAULT_WHISPER,
        Engine::Parakeet => DEFAULT_MODEL,
        Engine::Speaker => SPEAKER_MODEL,
    }
}

pub fn catalogue() -> &'static [ModelInfo] {
    CATALOGUE
}

pub fn find(id: &str) -> Option<&'static ModelInfo> {
    CATALOGUE.iter().find(|m| m.id == id)
}

/// Where model `m` lives under `models_dir`.
pub fn model_dir(models_dir: &Path, m: &ModelInfo) -> PathBuf {
    models_dir.join(m.id)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\'])
        && !matches!(name, "." | "..")
        && Path::new(name).components().count() == 1
        && Path::new(name).file_name().is_some_and(|part| part == name)
}

/// What a verified file looked like: `size mtime-seconds mtime-nanoseconds sha256`. Stored beside
/// it as `.<name>.verified` so a model of gigabytes is hashed once, not on every load; any change
/// to the file's size or modification time makes it hash again.
fn stamp(metadata: &std::fs::Metadata, sha: &str) -> Option<String> {
    let t = metadata.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(format!("{} {} {} {}", metadata.len(), t.as_secs(), t.subsec_nanos(), sha.to_ascii_lowercase()))
}

fn stamp_path(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy();
    Some(path.with_file_name(format!(".{name}.verified")))
}

fn valid_file(path: &Path, file: &ModelFile) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else { return false };
    if !valid_name(file.name) || !metadata.is_file() || metadata.len() != file.size {
        return false;
    }
    let expected = stamp(&metadata, file.sha256);
    let stamp_file = stamp_path(path);
    if let (Some(e), Some(sp)) = (&expected, &stamp_file)
        && std::fs::read_to_string(sp).is_ok_and(|s| s.trim() == e)
    {
        return true;
    }
    let Ok(mut input) = std::fs::File::open(path) else { return false };
    let mut hasher = sha2::Sha256::new();
    if std::io::copy(&mut input, &mut hasher).is_err() {
        return false;
    }
    let ok = format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(file.sha256);
    if ok && let (Some(e), Some(sp)) = (expected, stamp_file) {
        // best effort: without the stamp the file is only hashed again next time
        let _ = std::fs::write(sp, e);
    }
    ok
}

/// All files present and matching their pinned size and SHA-256.
pub fn installed(models_dir: &Path, m: &ModelInfo) -> bool {
    let d = model_dir(models_dir, m);
    m.files.iter().all(|f| valid_file(&d.join(f.name), f))
}

/// Bytes still to download for `m` (files already present are skipped).
pub fn missing_bytes(models_dir: &Path, m: &ModelInfo) -> u64 {
    let d = model_dir(models_dir, m);
    m.files.iter().filter(|f| !valid_file(&d.join(f.name), f)).map(|f| f.size).sum()
}

/// The tokenizer shared by the multilingual models has one checksum.
pub fn shared_tokenizer_sha() -> &'static str {
    TOKENIZER_MULTI
}

/// Download progress: `(bytes done, bytes total, file)`; return false to cancel.
#[cfg(feature = "download")]
pub type DownloadProgress<'a> = &'a mut dyn FnMut(u64, u64, &str) -> bool;

/// Download the missing files of `m` into `models_dir/<id>/`: each file streams into `<name>.part`,
/// its SHA-256 is checked, then it is renamed into place. A wrong checksum deletes the part file
/// and fails. Pure Rust TLS (rustls + RustCrypto) with the operating system's certificate
/// verifier.
#[cfg(feature = "download")]
pub fn download(models_dir: &Path, m: &ModelInfo, progress: DownloadProgress) -> Result<(), crate::SpeechError> {
    download_files(&model_dir(models_dir, m), m.files, progress)
}

/// [`download`] for any pinned file list (the neural voices use it too): missing files of
/// `files` go into `dir`, each verified against its SHA-256 and size before it is renamed into
/// place.
#[cfg(feature = "download")]
pub fn download_files(dir: &Path, files: &[ModelFile], progress: DownloadProgress) -> Result<(), crate::SpeechError> {
    use crate::SpeechError;
    use std::io::{Read, Write};
    if let Some(file) = files.iter().find(|file| !valid_name(file.name)) {
        return Err(SpeechError::Download(format!("invalid model artifact name: {:?}", file.name)));
    }
    std::fs::create_dir_all(dir)?;
    let total: u64 = files.iter().filter(|f| !valid_file(&dir.join(f.name), f)).map(|f| f.size).sum();
    let agent = agent();
    let mut done = 0u64;
    for f in files {
        let dest = dir.join(f.name);
        if valid_file(&dest, f) {
            continue;
        }
        let part = dir.join(format!("{}.part", f.name));
        // resume a part file left by an interrupted download (its bytes are hashed again)
        let mut hasher = sha2::Sha256::new();
        let mut got = 0u64;
        if let Ok(mut old) = std::fs::File::open(&part) {
            let len = old.metadata().map(|m| m.len()).unwrap_or(0);
            if len > 0 && len < f.size && std::io::copy(&mut old, &mut hasher).ok() == Some(len) {
                got = len;
            } else {
                hasher = sha2::Sha256::new();
            }
        }
        let start = f.offset.unwrap_or(0).saturating_add(got);
        let last = f.offset.unwrap_or(0).saturating_add(f.size).saturating_sub(1);
        let ranged = f.offset.is_some() || got > 0;
        let mut req = agent.get(f.url);
        if ranged {
            req = req.header("Range", &format!("bytes={start}-{last}"));
        }
        let resp = req.call().map_err(|e| SpeechError::Download(format!("{}: {e}", f.url)))?;
        if ranged && resp.status().as_u16() != 206 {
            return Err(SpeechError::Download(format!("{}: the server ignored the byte range (status {})", f.url, resp.status())));
        }
        let mut body = resp.into_body();
        let mut reader = body.as_reader();
        let file = std::fs::OpenOptions::new().create(true).append(got > 0).write(true).truncate(got == 0).open(&part)?;
        let mut out = std::io::BufWriter::new(file);
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = reader.read(&mut buf).map_err(|e| SpeechError::Download(format!("{}: {e}", f.name)))?;
            if n == 0 {
                break;
            }
            // never write past the pinned size, whatever the server sends
            let n = n.min(usize::try_from(f.size.saturating_sub(got)).unwrap_or(usize::MAX));
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
            got += n as u64;
            if !progress(done + got, total, f.name) {
                out.flush()?;
                // keep the part file: the next download resumes it
                return Err(SpeechError::Cancelled);
            }
        }
        out.flush()?;
        drop(out);
        let sha: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if sha != f.sha256 || got != f.size {
            let _ = std::fs::remove_file(&part);
            return Err(SpeechError::Download(format!("{}: checksum mismatch (got {sha}, {got} bytes; expected {}, {} bytes)", f.name, f.sha256, f.size)));
        }
        std::fs::rename(&part, &dest)?;
        // the bytes were just hashed: stamp the file so listing the models does not hash it again
        if let (Ok(m), Some(sp)) = (std::fs::metadata(&dest), stamp_path(&dest))
            && let Some(e) = stamp(&m, f.sha256)
        {
            let _ = std::fs::write(sp, e);
        }
        done += f.size;
    }
    Ok(())
}

#[cfg(feature = "download")]
fn agent() -> ureq::Agent {
    let provider = std::sync::Arc::new(rustls_rustcrypto::provider());
    ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .unversioned_rustls_crypto_provider(provider)
                .build(),
        )
        .timeout_connect(Some(std::time::Duration::from_secs(30)))
        .build()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_is_pinned_and_checksummed() {
        assert_eq!(find(DEFAULT_MODEL).map(|m| m.engine), Some(Engine::Parakeet));
        assert_eq!(find(DEFAULT_WHISPER).map(|m| m.engine), Some(Engine::Whisper));
        assert_eq!(find(SPEAKER_MODEL).map(|m| m.engine), Some(Engine::Speaker));
        for m in catalogue() {
            assert!(m.size() > 100_000_000, "{}", m.id);
            for f in m.files {
                assert!(f.url.starts_with("https://huggingface.co/openai/whisper-") || f.url.starts_with("https://huggingface.co/nvidia/"), "{}", f.url);
                assert!(f.url.contains("/resolve/"));
                if f.offset.is_none() {
                    assert!(f.url.ends_with(f.name));
                } else {
                    assert!(f.url.ends_with(".nemo"));
                }
                assert_eq!(f.sha256.len(), 64);
                assert!(f.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
                if f.name == "tokenizer.json" && m.engine == Engine::Whisper {
                    assert_eq!(f.sha256, shared_tokenizer_sha());
                }
            }
        }
    }

    #[test]
    fn installed_checks_sizes() {
        let dir = std::env::temp_dir().join(format!("filmcraft-speech-models-{}", std::process::id()));
        let m = find("whisper-tiny").unwrap();
        assert!(!installed(&dir, m));
        assert_eq!(missing_bytes(&dir, m), m.size());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn artifact_names_stay_inside_the_model_directory() {
        for name in ["../escape", "/tmp/escape", r"..\escape", r"C:\escape", "", ".", ".."] {
            assert!(!valid_name(name), "accepted {name:?}");
        }
        assert!(valid_name("model.safetensors"));
    }

    #[test]
    fn same_length_corruption_is_not_installed() {
        static FILES: &[ModelFile] = &[ModelFile {
            name: "tiny.bin",
            url: "https://example.invalid/tiny.bin",
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            size: 3,
            offset: None,
        }];
        let model = ModelInfo {
            id: "integrity-test",
            engine: Engine::Whisper,
            languages: &[],
            name: "integrity test",
            multilingual: false,
            description: "",
            license: "",
            license_url: "",
            author: "",
            source: "",
            files: FILES,
        };
        let root = tempfile::tempdir().unwrap();
        let directory = model_dir(root.path(), &model);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("tiny.bin"), b"bad").unwrap();
        assert!(!installed(root.path(), &model));
        assert_eq!(missing_bytes(root.path(), &model), 3);
        std::fs::write(directory.join("tiny.bin"), b"abc").unwrap();
        assert!(installed(root.path(), &model));
        // verified once: the stamp records it
        assert!(directory.join(".tiny.bin.verified").exists());
        assert!(installed(root.path(), &model));
        // a same-length change after verification (new modification time) is hashed again
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(directory.join("tiny.bin"), b"abd").unwrap();
        assert!(!installed(root.path(), &model));
        // a forged stamp for different content does not pass: its size or time differs
        std::fs::write(directory.join(".tiny.bin.verified"), "3 0 0 ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad").unwrap();
        assert!(!installed(root.path(), &model));
    }
}
