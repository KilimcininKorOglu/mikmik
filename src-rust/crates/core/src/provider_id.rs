// provider_id.rs — Branded newtypes for provider and model identifiers.
//
// ProviderId and ModelId are separate newtype wrappers around String so that
// the type system prevents accidentally passing a model name where a provider
// name is expected (and vice-versa).

use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::Deref;

// ---------------------------------------------------------------------------
// ProviderId
// ---------------------------------------------------------------------------

/// A branded identifier for an LLM provider (e.g. "anthropic", "openai").
///
/// Well-known constants are provided as associated constants so callers do
/// not need to hard-code raw strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderId(String);

impl ProviderId {
    /// Construct a `ProviderId` from any string-like value.
    pub fn new(s: impl Into<String>) -> Self {
        ProviderId(s.into())
    }

    // -----------------------------------------------------------------------
    // Well-known provider constants
    // -----------------------------------------------------------------------

    pub const ANTHROPIC: &'static str = "anthropic";
    pub const OPENAI: &'static str = "openai";
    pub const GOOGLE: &'static str = "google";
    pub const GOOGLE_VERTEX: &'static str = "google-vertex";
    pub const AMAZON_BEDROCK: &'static str = "amazon-bedrock";
    pub const AZURE: &'static str = "azure";
    pub const GITHUB_COPILOT: &'static str = "github-copilot";
    pub const MISTRAL: &'static str = "mistral";
    pub const XAI: &'static str = "xai";
    pub const GROQ: &'static str = "groq";
    pub const DEEPINFRA: &'static str = "deepinfra";
    pub const CEREBRAS: &'static str = "cerebras";
    pub const COHERE: &'static str = "cohere";
    pub const CROF: &'static str = "crof";
    pub const TOGETHER_AI: &'static str = "together-ai";
    pub const PERPLEXITY: &'static str = "perplexity";
    pub const OPENROUTER: &'static str = "openrouter";
    pub const OLLAMA: &'static str = "ollama";
    pub const LM_STUDIO: &'static str = "lm-studio";
    pub const LLAMA_CPP: &'static str = "llama-cpp";
    /// Apple MLX inference server, reached over its OpenAI-compatible endpoint.
    pub const MLX_LM: &'static str = "mlx-lm";
    /// User-supplied endpoint speaking the OpenAI wire format.
    pub const CUSTOM_OPENAI: &'static str = "custom-openai";
    /// User-supplied endpoint speaking the Anthropic wire format.
    ///
    /// Separate from [`ANTHROPIC`](Self::ANTHROPIC) so a custom gateway can sit
    /// alongside the real one instead of replacing it.
    pub const CUSTOM_ANTHROPIC: &'static str = "custom-anthropic";
    pub const DEEPSEEK: &'static str = "deepseek";
    pub const GITLAB: &'static str = "gitlab";
    pub const CLOUDFLARE: &'static str = "cloudflare";
    pub const VENICE: &'static str = "venice";
    pub const SAP: &'static str = "sap";
    pub const SAMBANOVA: &'static str = "sambanova";
    pub const HUGGINGFACE: &'static str = "huggingface";
    pub const NVIDIA: &'static str = "nvidia";
    pub const SILICONFLOW: &'static str = "siliconflow";
    pub const MOONSHOT: &'static str = "moonshotai";
    pub const ZHIPU: &'static str = "zhipuai";
    pub const ZAI: &'static str = "zai";
    pub const NEBIUS: &'static str = "nebius";
    pub const OVHCLOUD: &'static str = "ovhcloud";
    pub const SCALEWAY: &'static str = "scaleway";
    pub const VULTR: &'static str = "vultr";
    pub const BASETEN: &'static str = "baseten";
    pub const FRIENDLI: &'static str = "friendli";
    pub const UPSTAGE: &'static str = "upstage";
    pub const STEPFUN: &'static str = "stepfun";
    pub const FIREWORKS: &'static str = "fireworks";
    pub const NOVITA: &'static str = "novita";
    pub const MINIMAX: &'static str = "minimax";
    pub const CODEX: &'static str = "codex";
    pub const OPENCODE_GO: &'static str = "opencode-go";
    pub const OPENCODE_ZEN: &'static str = "opencode-zen";
    pub const SYNTHETIC: &'static str = "synthetic";
    pub const ROUTING: &'static str = "routing";
    pub const NEURALWATT: &'static str = "neuralwatt";
    pub const FREE: &'static str = "free";
    // omp-parity API-key providers.
    pub const META: &'static str = "meta";
    pub const COREWEAVE: &'static str = "coreweave";
    pub const SAKANA: &'static str = "sakana";
    pub const GMI_CLOUD: &'static str = "gmi-cloud";
    pub const NANOGPT: &'static str = "nanogpt";
    pub const ZENMUX: &'static str = "zenmux";
    pub const VERCEL_AI_GATEWAY: &'static str = "vercel-ai-gateway";
    pub const UMANS: &'static str = "umans";
    pub const QIANFAN: &'static str = "qianfan";
    pub const WAFER_SERVERLESS: &'static str = "wafer-serverless";

    /// Every provider id mikmik ships with, including the spelling aliases
    /// users type (`lmstudio` for `lm-studio`, `zhipu` for `zhipuai`, …).
    ///
    /// Used to decide whether the first segment of a `"<account>/<model>"`
    /// string names an account or is part of the model id itself. A model id
    /// may legitimately contain a slash (`meta-llama/Llama-3.3` on OpenRouter),
    /// so the segment is only treated as an account when it appears here or in
    /// the user's own `providers` map.
    pub const WELL_KNOWN: &'static [&'static str] = &[
        Self::ANTHROPIC,
        Self::OPENAI,
        Self::GOOGLE,
        Self::GOOGLE_VERTEX,
        Self::AMAZON_BEDROCK,
        Self::AZURE,
        Self::GITHUB_COPILOT,
        Self::MISTRAL,
        Self::XAI,
        Self::GROQ,
        Self::DEEPINFRA,
        Self::CEREBRAS,
        Self::COHERE,
        Self::CROF,
        Self::TOGETHER_AI,
        Self::PERPLEXITY,
        Self::OPENROUTER,
        Self::OLLAMA,
        Self::LM_STUDIO,
        Self::LLAMA_CPP,
        Self::MLX_LM,
        Self::CUSTOM_OPENAI,
        Self::CUSTOM_ANTHROPIC,
        Self::DEEPSEEK,
        Self::GITLAB,
        Self::CLOUDFLARE,
        Self::VENICE,
        Self::SAP,
        Self::SAMBANOVA,
        Self::HUGGINGFACE,
        Self::NVIDIA,
        Self::SILICONFLOW,
        Self::MOONSHOT,
        Self::ZHIPU,
        Self::ZAI,
        Self::NEBIUS,
        Self::OVHCLOUD,
        Self::SCALEWAY,
        Self::VULTR,
        Self::BASETEN,
        Self::FRIENDLI,
        Self::UPSTAGE,
        Self::STEPFUN,
        Self::FIREWORKS,
        Self::NOVITA,
        Self::MINIMAX,
        Self::CODEX,
        Self::OPENCODE_GO,
        Self::OPENCODE_ZEN,
        Self::SYNTHETIC,
        Self::ROUTING,
        Self::NEURALWATT,
        Self::FREE,
        Self::META,
        Self::COREWEAVE,
        Self::SAKANA,
        Self::GMI_CLOUD,
        Self::NANOGPT,
        Self::ZENMUX,
        Self::VERCEL_AI_GATEWAY,
        Self::UMANS,
        Self::QIANFAN,
        Self::WAFER_SERVERLESS,
        // Spelling aliases accepted on the wire but not canonical ids.
        "openai-codex",
        "lmstudio",
        "llamacpp",
        "llama-server",
        "mlxlm",
        "togetherai",
        "qwen",
        "alibaba",
        "moonshot",
        "zhipu",
        "vultr-ai",
    ];

    /// Whether `id` names a provider mikmik ships with.
    pub fn is_well_known(id: &str) -> bool {
        Self::WELL_KNOWN.contains(&id)
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Deref for ProviderId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<String> for ProviderId {
    fn from(s: String) -> Self {
        ProviderId(s)
    }
}

impl From<&str> for ProviderId {
    fn from(s: &str) -> Self {
        ProviderId(s.to_string())
    }
}

impl PartialEq<str> for ProviderId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ProviderId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

// ---------------------------------------------------------------------------
// ModelId
// ---------------------------------------------------------------------------

/// A branded identifier for a model (e.g. "claude-opus-4-5", "gpt-4o").
///
/// Kept separate from `ProviderId` for type safety — you cannot accidentally
/// pass a model name where a provider name is expected.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelId(String);

impl ModelId {
    /// Construct a `ModelId` from any string-like value.
    pub fn new(s: impl Into<String>) -> Self {
        ModelId(s.into())
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Deref for ModelId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<String> for ModelId {
    fn from(s: String) -> Self {
        ModelId(s)
    }
}

impl From<&str> for ModelId {
    fn from(s: &str) -> Self {
        ModelId(s.to_string())
    }
}

impl PartialEq<str> for ModelId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ModelId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}
