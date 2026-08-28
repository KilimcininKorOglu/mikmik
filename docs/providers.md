# LLM Providers

MikMik supports a wide range of LLM providers through a unified provider abstraction. Every provider implements the same `LlmProvider` trait, so switching between them requires only a configuration change.

> Running a model on your own machine (llama.cpp, LM Studio, Ollama, vLLM)? See
> the dedicated [Local Models](local-models) guide for recommended server flags,
> tool-calling setup, model guidance, and cache accounting.

---

## Selecting a Provider

Use the `--provider` flag on any invocation to override the active provider:

```
mikmik --provider openai "refactor this module"
mikmik --provider ollama "explain this function"
mikmik --provider groq --model llama-3.3-70b-versatile "write tests"
```

The provider can also be set persistently in `~/.config/mikmik/settings.json`:

```json
{
  "provider": "openai"
}
```

When no provider is specified, MikMik defaults to **Anthropic**.

---

## Provider Reference

### Anthropic (default)

The default provider. Uses the `/v1/messages` streaming endpoint.

**Authentication:** `ANTHROPIC_API_KEY` environment variable, or set `api_key` in `settings.json`.

**Default model:** `claude-sonnet-4-6`

**Available models (bundled snapshot):**

| Model ID                    | Context Window | Max Output | Input ($/1M) | Output ($/1M) |
|-----------------------------|----------------|------------|--------------|---------------|
| `claude-opus-4-6`           | 200,000        | 32,000     | $15.00       | $75.00        |
| `claude-sonnet-4-6`         | 200,000        | 16,000     | $3.00        | $15.00        |
| `claude-haiku-4-5-20251001` | 200,000        | 8,096      | $0.80        | $4.00         |

All Anthropic models support tool calling, vision, and extended reasoning.

**Configuration:**

```json
{
  "provider": "anthropic",
  "providers": {
    "anthropic": {
      "api_key": "sk-ant-...",
      "models_whitelist": ["claude-sonnet-4-6", "claude-haiku-4-5-20251001"]
    }
  }
}
```

**Base URL override:** Set `ANTHROPIC_BASE_URL` to point at a proxy or local mirror.

---

### OpenAI

Uses the OpenAI Chat Completions API (`/v1/chat/completions`).

**Authentication:** `OPENAI_API_KEY` environment variable.

**Default model:** `gpt-4o`

**Available models (bundled snapshot):**

| Model ID      | Context Window | Max Output | Reasoning |
|---------------|----------------|------------|-----------|
| `gpt-4o`      | 128,000        | 16,384     | No        |
| `gpt-4o-mini` | 128,000        | 16,384     | No        |
| `o3`          | 200,000        | 100,000    | Yes       |
| `o4-mini`     | 200,000        | 100,000    | Yes       |

**Configuration:**

```json
{
  "provider": "openai",
  "providers": {
    "openai": {
      "api_key": "sk-...",
      "api_base": "https://api.openai.com/v1"
    }
  }
}
```

---

### Google (Gemini)

Uses the Google Generative Language / Vertex AI API.

**Authentication:** `GOOGLE_API_KEY` environment variable (for AI Studio) or `GOOGLE_APPLICATION_CREDENTIALS` (for Vertex AI).

**Default model:** `gemini-2.5-flash`

**Available models (bundled snapshot):**

| Model ID           | Context Window | Max Output |
|--------------------|----------------|------------|
| `gemini-2.5-pro`   | 1,048,576      | 65,536     |
| `gemini-2.5-flash` | 1,048,576      | 65,536     |
| `gemini-2.0-flash` | 1,048,576      | 8,192      |

**Configuration:**

```json
{
  "provider": "google",
  "providers": {
    "google": {
      "api_key": "AIza..."
    }
  }
}
```

---

### Azure OpenAI

Uses the Azure OpenAI Chat Completions endpoint. The deployment name acts as the model identifier.

**Authentication:** Three environment variables are required:
- `AZURE_API_KEY` — your Azure OpenAI API key
- `AZURE_RESOURCE_NAME` — your Azure resource name (the subdomain of `.openai.azure.com`)
- `AZURE_API_VERSION` — API version (defaults to `2024-08-01-preview`)

**Default model:** `gpt-4o`

**Request URL format:**

```
https://{AZURE_RESOURCE_NAME}.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={version}
```

**Configuration:**

```json
{
  "provider": "azure",
  "providers": {
    "azure": {
      "api_key": "...",
      "options": {
        "resource_name": "my-azure-resource",
        "api_version": "2024-08-01-preview"
      }
    }
  }
}
```

---

### AWS Bedrock

Uses the Bedrock Converse Streaming API. Supports all Claude models deployed on Bedrock.

**Authentication (two modes):**

1. **Bearer token:** Set `AWS_BEARER_TOKEN_BEDROCK` (takes priority over SigV4).
2. **SigV4 credentials:** Set `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and optionally `AWS_SESSION_TOKEN`.

**Region:** Reads `AWS_REGION` or `AWS_DEFAULT_REGION` (defaults to `us-east-1`).

**Default model:** `anthropic.claude-sonnet-4-6-v1`

The adapter automatically prepends regional cross-inference prefixes (e.g. `us.anthropic.claude-...`) for US-region deployments.

**Configuration:**

```json
{
  "provider": "amazon-bedrock",
  "providers": {
    "amazon-bedrock": {
      "options": {
        "region": "us-east-1"
      }
    }
  }
}
```

---

### GitHub Copilot

Uses the GitHub Copilot Chat Completions API (`https://api.githubcopilot.com/chat/completions`).

**Authentication:** `GITHUB_TOKEN` environment variable.

**Default model:** `gpt-4o`

**Configuration:**

```json
{
  "provider": "github-copilot",
  "providers": {
    "github-copilot": {
      "api_key": "ghu_..."
    }
  }
}
```

---

### Cohere

Native Cohere API adapter.

**Authentication:** `COHERE_API_KEY` environment variable.

**Default model:** `command-r-plus`

**Configuration:**

```json
{
  "provider": "cohere",
  "providers": {
    "cohere": {
      "api_key": "..."
    }
  }
}
```

---

### MiniMax

The built-in provider uses the Anthropic-compatible Messages API.

**Authentication:** `MINIMAX_API_KEY` environment variable, or set `api_key` in `settings.json`.

**Default model:** `MiniMax-M3`

| Model          | Context window | Input modalities   | Thinking                                           |
|----------------|---------------:|--------------------|----------------------------------------------------|
| `MiniMax-M3`   |      1,000,000 | Text, image, video | Off by default; supports `adaptive` and `disabled` |
| `MiniMax-M2.7` |        204,800 | Text               | Always on                                          |

The catalog retains the model's complete input-modality metadata. MikMik's built-in attachment flow currently sends text and image blocks.

Pricing is in USD per million tokens:

| Model          | Service tier |  Input range | Input | Output | Cache read |   Cache write |
|----------------|--------------|-------------:|------:|-------:|-----------:|--------------:|
| `MiniMax-M3`   | Standard     |   Up to 512k | $0.30 |  $1.20 |      $0.06 | Not published |
| `MiniMax-M3`   | Standard     |    Over 512k | $0.60 |  $2.40 |      $0.12 | Not published |
| `MiniMax-M3`   | Priority     |   Up to 512k | $0.45 |  $1.80 |      $0.09 | Not published |
| `MiniMax-M3`   | Priority     |    Over 512k | $0.90 |  $3.60 |      $0.18 | Not published |
| `MiniMax-M2.7` | Standard     | All requests | $0.30 |  $1.20 |      $0.06 |        $0.375 |

| Protocol          | Global base URL                    | China base URL                       | Path added by MikMik |
|-------------------|------------------------------------|--------------------------------------|-----------------------|
| Anthropic         | `https://api.minimax.io/anthropic` | `https://api.minimaxi.com/anthropic` | `/v1/messages`        |
| OpenAI-compatible | `https://api.minimax.io/v1`        | `https://api.minimaxi.com/v1`        | `/chat/completions`   |

The built-in `minimax` provider uses the Anthropic row. To use the China endpoint, set `MINIMAX_BASE_URL` or configure `api_base`:

```json
{
  "provider": "minimax",
  "model": "MiniMax-M3",
  "providers": {
    "minimax": {
      "api_key": "...",
      "api_base": "https://api.minimaxi.com/anthropic"
    }
  }
}
```

MiniMax-M3 uses the standard service tier by default. To request priority admission, set `service_tier` in the provider options:

```json
{
  "provider": "minimax",
  "model": "MiniMax-M3",
  "providers": {
    "minimax": {
      "api_key": "...",
      "options": {
        "service_tier": "priority"
      }
    }
  }
}
```

For the OpenAI-compatible protocol, use the custom provider with the corresponding `/v1` base URL:

```json
{
  "provider": "custom-openai",
  "model": "MiniMax-M3",
  "providers": {
    "custom-openai": {
      "api_key": "...",
      "api_base": "https://api.minimax.io/v1"
    }
  }
}
```

#### MiniMax Token Plan (coding plan)

The Token Plan is a subscription that serves MiniMax models over the OpenAI-compatible `/v1` route. It is a separate provider from the Anthropic-wire `minimax` above, with its own login and key:

| Provider id       | Region        | Base URL                      | Key environment variable    | Subscribe                                     |
|-------------------|---------------|-------------------------------|-----------------------------|-----------------------------------------------|
| `minimax-code`    | International | `https://api.minimax.io/v1`   | `MINIMAX_CODE_API_KEY`      | `https://platform.minimax.io/subscribe/token-plan` |
| `minimax-code-cn` | China         | `https://api.minimaxi.com/v1` | `MINIMAX_CODE_CN_API_KEY`   | `https://platform.minimaxi.com/subscribe/token-plan` |

Both are offered under "Other" in [`/connect`](commands.md#connect): subscribe to the Token Plan, then paste the key it gives you.

---

### Kimi Code

Moonshot's coding plan, signed in with the OAuth 2.0 device authorization grant rather than an API key. It is a separate provider from the API-key `moonshotai` route: it serves an OpenAI-compatible API at `https://api.kimi.com/coding/v1` behind a device-flow token.

Pick "Kimi Code" under "Other" in [`/connect`](commands.md#connect). MikMik opens `auth.kimi.com` in the browser and shows a user code; approve it there and the session switches to the new account. The access token is refreshed automatically when it expires. A second login files a second account named after the token's identity.

| Setting                 | Environment variable    | Default                          |
|-------------------------|-------------------------|----------------------------------|
| OAuth host              | `KIMI_CODE_OAUTH_HOST` (or `KIMI_OAUTH_HOST`) | `https://auth.kimi.com` |
| API base                | `KIMI_CODE_BASE_URL`    | `https://api.kimi.com/coding/v1` |

The overrides exist for self-hosted or regional gateways; the defaults need no configuration.

---

### Xiaomi MiMo

OpenAI-compatible models at `https://api.xiaomimimo.com/v1`, reached with an API key (`XIAOMI_API_KEY`). Both a standard pay-as-you-go `sk-` key and a Token Plan `tp-` key work.

Pick "Xiaomi MiMo" under "Other" in [`/connect`](commands.md#connect) and paste the key from the [console](https://platform.xiaomimimo.com). The Token Plan is served from regional hosts; to target one, override the base URL:

```json
{
  "provider": "xiaomi",
  "providers": {
    "xiaomi": {
      "api_key": "tp-...",
      "api_base": "https://token-plan-sgp.xiaomimimo.com/v1"
    }
  }
}
```

Regional Token Plan bases: `token-plan-sgp` (Singapore), `token-plan-ams` (Europe), `token-plan-cn` (China).

---

### xAI Grok (SuperGrok OAuth)

SuperGrok and X Premium+ subscribers can sign in with the OAuth 2.0 device authorization grant instead of an API key. It is a separate provider (`xai-oauth`) from the API-key `xai` route, but both serve the same OpenAI-compatible `https://api.x.ai/v1` endpoint.

Pick "xAI Grok (SuperGrok)" under "Other" in [`/connect`](commands.md#connect). MikMik requests a device code, opens `auth.x.ai` in the browser, and shows a user code; approve it there and the session switches to the new account. The access token is refreshed automatically (the token endpoint is discovered from xAI's OIDC document). `XAI_OAUTH_BASE_URL` overrides the inference base if needed.

The subscription bills through xAI directly; this is the same account you use with the official Grok CLI.

---

### GitLab Duo

GitLab Duo is reached in two steps. Sign in with the OAuth 2.0 PKCE flow (browser plus a loopback callback on `localhost:8080`) or supply a Personal Access Token in `GITLAB_TOKEN`. Either way MikMik holds a GitLab access token, which it exchanges at request time for a short-lived direct-access token and gateway headers; those authenticate the OpenAI-compatible proxy at `https://cloud.gitlab.com/ai/v1/proxy/openai/v1`.

Pick "GitLab Duo" under "Other" in [`/connect`](commands.md#connect). MikMik opens `gitlab.com` in the browser; approve the authorization and the session switches to the new account. Duo must be enabled for the account, or the direct-access exchange returns 403.

The bundled OAuth client id is registered with the `http://localhost:8080/callback` redirect URI. If GitLab rejects the authorize request ("The redirect URI included is not valid"), either register your own GitLab OAuth application and set `GITLAB_CLIENT_ID` + `GITLAB_REDIRECT_URI`, or skip OAuth and set `GITLAB_TOKEN` to a Personal Access Token with the `api` scope.

---

### Custom endpoints

Two slots exist for endpoints MikMik does not ship a provider for, one per wire format:

| Provider id         | Wire format | Base URL setting                          | Key environment variable   |
|---------------------|-------------|-------------------------------------------|----------------------------|
| `custom-openai`     | OpenAI      | `providers."custom-openai".api_base`      | `CUSTOM_OPENAI_API_KEY`    |
| `custom-anthropic`  | Anthropic   | `providers."custom-anthropic".api_base`   | `CUSTOM_ANTHROPIC_API_KEY` |

Both are offered under "Advanced" in [`/connect`](commands.md#connect), which asks for the URL and the key and writes them for you.

`custom-anthropic` is a separate provider, not a redirect of the built-in one. Pointing `providers.anthropic.api_base` at a gateway *replaces* Anthropic for the session; configuring `custom-anthropic` adds a second entry, so both appear in [`/model`](commands.md#model) and you can move between them.

```json
{
  "providers": {
    "custom-anthropic": {
      "api_key": "...",
      "api_base": "https://gateway.example/v1"
    }
  }
}
```

`CUSTOM_ANTHROPIC_BASE_URL` sets the base URL from the environment instead. Without a base URL the provider is not registered at all, since there would be nothing to point it at. The key may be left empty for an unauthenticated self-hosted gateway.

Neither slot has models.dev catalog entries. The picker shows whatever the endpoint reports from live discovery, falling back to a single `default` row; use [`modelOverrides`](#overriding-model-metadata) to name models and correct their context windows.

---

### Ollama

Connects to a locally running Ollama instance. No API key required.

**Base URL:** Reads `OLLAMA_HOST` (defaults to `http://localhost:11434`). MikMik appends `/v1` to construct the OpenAI-compatible endpoint. For a remote Ollama server, set `providers.ollama.api_base` to that host: both `http://192.168.1.50:11434` and `http://192.168.1.50:11434/v1` work, because the `/v1` suffix is added when it is missing.

**Default model:** `llama3.2`

**Model list:** When using `/connect` or `/model`, the picker queries your Ollama server via `/api/tags` and shows only the models you have installed (`ollama list`). Cloud models (e.g., `kimi-k2.6:cloud`) appear after you run `ollama pull <model>:cloud`.

`/api/tags` and the health check use the native Ollama API rather than the OpenAI-compatible one, and they follow `api_base` to the same host. A remote `api_base` therefore lists the remote server's models, not whatever happens to run on localhost.

**Configuration:**

```json
{
  "provider": "ollama",
  "providers": {
    "ollama": {
      "api_base": "http://localhost:11434"
    }
  }
}
```

Run a model locally first with `ollama pull llama3.2`, then:

```
mikmik --provider ollama --model llama3.2 "explain this code"
```

---

### LM Studio (local)

Connects to a locally running LM Studio server. No API key required.

**Base URL:** Reads `LM_STUDIO_HOST` (defaults to `http://localhost:1234`). MikMik appends `/v1`. A remote server is configured the same way as Ollama's, through `providers.lmstudio.api_base`, with or without the `/v1` suffix.

**Default model:** `default` (whichever model is loaded in LM Studio)

**Model list:** The picker reads LM Studio's native API, which follows `api_base` to the same host, and lists the models currently loaded there.

**Configuration:**

```json
{
  "provider": "lmstudio",
  "providers": {
    "lmstudio": {
      "api_base": "http://localhost:1234/v1"
    }
  }
}
```

---

### LLaMA.cpp (local)

Connects to a locally running llama.cpp HTTP server. No API key required.

**Base URL:** Reads `LLAMA_CPP_HOST` (defaults to `http://localhost:8080`). MikMik appends `/v1`.

**Default model:** `default`

**Configuration:**

```json
{
  "provider": "llamacpp",
  "providers": {
    "llamacpp": {
      "api_base": "http://localhost:8080/v1"
    }
  }
}
```

Start llama.cpp with the `--server` flag before use.

For recommended `llama-server` flags (tool calling, context sizing, prompt
caching), model guidance, and cache-accounting details, see the
[Local Models](local-models) guide.

---

### MLX LM (local)

Connects to a locally running `mlx_lm.server`. Runs on Apple Silicon, so the
entry appears in the `/connect` list only on macOS. No API key required unless
the server was started with `--api-key`.

**Base URL:** Reads `MLX_LM_HOST` (defaults to `http://localhost:8080`, the port
`mlx_lm.server` binds without a `--port` flag). MikMik appends `/v1`.

**Default model:** taken from the server's own `GET /v1/models`.

**Configuration:**

```json
{
  "provider": "mlxlm",
  "providers": {
    "mlxlm": {
      "api_base": "http://localhost:8080/v1"
    }
  }
}
```

Start the server before connecting, naming the model to load:

```bash
mlx_lm.server --model mlx-community/Qwen3.5-9B-MLX-4bit
```

`llama.cpp` defaults to the same port, so move one of them if both run at once:

```bash
mlx_lm.server --model <model> --port 9000
MLX_LM_HOST=http://localhost:9000 mikmik --provider mlxlm "hello"
```

Writing the `providers.mlxlm` block by hand works on any platform, which is how
a Linux or Windows machine reaches an `mlx_lm.server` running on a Mac.

---

### Groq

Fast inference cloud with OpenAI-compatible API.

**Authentication:** `GROQ_API_KEY` environment variable.

**Base URL:** `https://api.groq.com/openai/v1`

**Default model:** `llama-3.3-70b-versatile`

**Configuration:**

```json
{
  "provider": "groq",
  "providers": {
    "groq": {
      "api_key": "gsk_..."
    }
  }
}
```

---

### DeepSeek

OpenAI-compatible API with extended reasoning output via a `reasoning_content` field.

**Authentication:** `DEEPSEEK_API_KEY` environment variable.

**Base URL:** `https://api.deepseek.com/v1`

**Default model:** `deepseek-chat`

**Configuration:**

```json
{
  "provider": "deepseek",
  "providers": {
    "deepseek": {
      "api_key": "sk-..."
    }
  }
}
```

---

### Mistral AI

OpenAI-compatible API with Mistral-specific protocol quirks (tool call ID formatting, tool-user sequence injection).

**Authentication:** `MISTRAL_API_KEY` environment variable.

**Base URL:** `https://api.mistral.ai/v1`

**Default model:** `mistral-large-latest`

**Configuration:**

```json
{
  "provider": "mistral",
  "providers": {
    "mistral": {
      "api_key": "..."
    }
  }
}
```

---

### xAI (Grok)

**Authentication:** `XAI_API_KEY` environment variable.

**Base URL:** `https://api.x.ai/v1`

**Default model:** `grok-2`

**Configuration:**

```json
{
  "provider": "xai",
  "providers": {
    "xai": {
      "api_key": "xai-..."
    }
  }
}
```

---

### OpenRouter

Unified API gateway to many models. Sends `HTTP-Referer: https://mikmik.ai/` and `X-Title: MikMik` headers automatically.

**Authentication:** `OPENROUTER_API_KEY` environment variable.

**Base URL:** `https://openrouter.ai/api/v1`

**Default model:** `anthropic/claude-sonnet-4`

Model identifiers use OpenRouter's routing format: `provider/model-name`.

**Configuration:**

```json
{
  "provider": "openrouter",
  "providers": {
    "openrouter": {
      "api_key": "sk-or-..."
    }
  }
}
```

---

### Together AI

Hosted open-source models.

**Authentication:** `TOGETHER_API_KEY` environment variable.

**Base URL:** `https://api.together.xyz/v1`

**Default model:** `meta-llama/Llama-3.3-70B-Instruct-Turbo`

**Configuration:**

```json
{
  "provider": "togetherai",
  "providers": {
    "togetherai": {
      "api_key": "..."
    }
  }
}
```

---

### Perplexity

Search-augmented LLM API.

**Authentication:** `PERPLEXITY_API_KEY` environment variable.

**Base URL:** `https://api.perplexity.ai`

**Default model:** `sonar-pro`

**Configuration:**

```json
{
  "provider": "perplexity",
  "providers": {
    "perplexity": {
      "api_key": "pplx-..."
    }
  }
}
```

---

### DeepInfra

Hosted open-weight models on OpenAI-compatible API.

**Authentication:** `DEEPINFRA_API_KEY` environment variable.

**Base URL:** `https://api.deepinfra.com/v1/openai`

**Default model:** `meta-llama/Llama-3.3-70B-Instruct`

**Configuration:**

```json
{
  "provider": "deepinfra",
  "providers": {
    "deepinfra": {
      "api_key": "..."
    }
  }
}
```

---

### Venice AI

Privacy-focused inference.

**Authentication:** `VENICE_API_KEY` environment variable.

**Base URL:** `https://api.venice.ai/api/v1`

**Default model:** `llama-3.3-70b` (resolved from the model registry at runtime)

**Configuration:**

```json
{
  "provider": "venice",
  "providers": {
    "venice": {
      "api_key": "..."
    }
  }
}
```

---

### Cerebras

Wafer-scale inference hardware.

**Authentication:** `CEREBRAS_API_KEY` environment variable.

**Base URL:** `https://api.cerebras.ai/v1`

**Default model:** `llama-3.3-70b`

**Configuration:**

```json
{
  "provider": "cerebras",
  "providers": {
    "cerebras": {
      "api_key": "..."
    }
  }
}
```

---

### The rest

These speak the OpenAI wire format and need no configuration beyond a key. Each
is registered under its id and reads one environment variable:

| Provider id     | Name          | Key variable           | Base URL                                          |
|-----------------|---------------|------------------------|---------------------------------------------------|
| `baseten`       | Baseten       | `BASETEN_API_KEY`      | `https://inference.baseten.co/v1`                 |
| `crof`          | crof          | `CROF_API_KEY`         | `https://api.crof.ai/v1`                          |
| `fireworks`     | Fireworks AI  | `FIREWORKS_API_KEY`    | `https://api.fireworks.ai/inference/v1`           |
| `friendli`      | Friendli      | `FRIENDLI_TOKEN`       | `https://api.friendli.ai/serverless/v1`           |
| `huggingface`   | Hugging Face  | `HF_TOKEN`             | `https://router.huggingface.co/v1`                |
| `moonshotai`    | Moonshot AI   | `MOONSHOT_API_KEY`     | `https://api.moonshot.ai/v1`                      |
| `nebius`        | Nebius        | `NEBIUS_API_KEY`       | `https://api.tokenfactory.nebius.com/v1`          |
| `neuralwatt`    | NeuralWatt    | `NEURALWATT_API_KEY`   | `https://api.neuralwatt.com/v1`                   |
| `novita`        | Novita        | `NOVITA_API_KEY`       | `https://api.novita.ai/v3/openai`                 |
| `nvidia`        | Nvidia NIM    | `NVIDIA_API_KEY`       | `https://integrate.api.nvidia.com/v1`             |
| `qwen`          | Qwen (Alibaba)| `DASHSCOPE_API_KEY`    | `https://dashscope.aliyuncs.com/compatible-mode/v1`|
| `opencode-go`   | OpenCode Go   | `OPENCODE_API_KEY`     | `https://opencode.ai/zen/go/v1`                   |
| `opencode-zen`  | OpenCode Zen  | `OPENCODE_API_KEY`     | `https://opencode.ai/zen/v1`                      |
| `ovhcloud`      | OVHcloud      | `OVHCLOUD_API_KEY`     | `https://oai.endpoints.kepler.ai.cloud.ovh.net/v1`|
| `routing`       | routing.run   | `ROUTING_API_KEY`      | `https://api.routing.run/v1`                      |
| `sambanova`     | SambaNova     | `SAMBANOVA_API_KEY`    | `https://api.sambanova.ai/v1`                     |
| `scaleway`      | Scaleway      | `SCALEWAY_API_KEY`     | `https://api.scaleway.ai/v1`                      |
| `siliconflow`   | SiliconFlow   | `SILICONFLOW_API_KEY`  | `https://api.siliconflow.com/v1`                  |
| `stepfun`       | stepfun       | `STEPFUN_API_KEY`      | `https://api.stepfun.com/v1`                      |
| `synthetic`     | Synthetic.dev | `SYNTHETIC_API_KEY`    | `https://api.synthetic.new/openai/v1`             |
| `upstage`       | Upstage       | `UPSTAGE_API_KEY`      | `https://api.upstage.ai/v1/solar`                 |
| `vultr`         | Vultr         | `VULTR_API_KEY`        | `https://api.vultrinference.com/v1`               |
| `zai`           | Z.AI          | `ZAI_API_KEY`          | `https://api.z.ai/api/coding/paas/v4`             |
| `zhipuai`       | Zhipu AI      | `ZHIPU_API_KEY`        | `https://open.bigmodel.cn/api/paas/v4`            |

`api_base` in the provider's `settings.json` entry overrides the base URL.
`alibaba` is accepted as an alias for `qwen`.

Three more have their own handling: `codex` (OpenAI ChatGPT subscription, see
[Authentication](auth)), `google-vertex`, and `free`.

Four local providers accept their id with or without the hyphen, so
`together-ai` and `togetherai`, `lm-studio` and `lmstudio`, `llama-cpp` and
`llamacpp` (plus `llama-server`), and `mlx-lm` and `mlxlm` all resolve to the
same provider.

---

## Per-Provider Configuration in settings.json

The `providers` map in `~/.config/mikmik/settings.json` accepts per-provider `ProviderConfig` objects:

```json
{
  "provider": "anthropic",
  "providers": {
    "anthropic": {
      "api_key": "sk-ant-...",
      "api_base": "https://api.anthropic.com",
      "enabled": true,
      "models_whitelist": [],
      "models_blacklist": [],
      "options": {}
    },
    "openai": {
      "api_key": "sk-...",
      "enabled": true
    },
    "ollama": {
      "enabled": true,
      "api_base": "http://192.168.1.50:11434/v1"
    }
  }
}
```

**Fields:**

| Field              | Type             | Description                                      |
|--------------------|------------------|--------------------------------------------------|
| `api_key`          | string           | Override the environment variable API key        |
| `api_base`         | string           | Override the default base URL                    |
| `protocol`         | string           | Wire format this account speaks                  |
| `enabled`          | bool             | Enable or disable the provider (default: `true`) |
| `models_whitelist` | array of strings | If non-empty, only listed model IDs are allowed  |
| `models_blacklist` | array of strings | Listed model IDs are refused                     |
| `options`          | object           | Provider-specific pass-through options           |

An entry named after its vendor (`"ollama"`, `"openai"`) needs no `protocol`; the name is the wire format. Name an entry anything else to run a second account of the same vendor, and give it `protocol` so it is still built by the right implementation:

```json
{
  "providers": {
    "home-ollama": {
      "protocol": "ollama",
      "api_base": "http://192.168.1.50:11434"
    }
  }
}
```

`api_base` is shaped for that protocol, not for the entry's name: an OpenAI-compatible endpoint gains the `/v1` suffix when it is missing, and an `openai` one has it removed because that implementation appends `/v1` itself.

## Model Whitelist and Blacklist

When `models_whitelist` is non-empty for a provider, only the listed model IDs can be selected for that provider. Any model ID in `models_blacklist` is rejected regardless of the whitelist:

```json
{
  "providers": {
    "openai": {
      "models_whitelist": ["gpt-4o", "gpt-4o-mini"],
      "models_blacklist": ["gpt-4o-mini"]
    }
  }
}
```

The above example allows only `gpt-4o` (whitelist minus blacklist).

## Model Registry

MikMik ships a bundled snapshot of models for Anthropic, OpenAI, and Google. At runtime it optionally refreshes from the public `https://models.dev/api.json` API (cached to `~/.config/mikmik/models_cache.json`, refreshed at most every 5 minutes). Network failures are swallowed silently; the bundled snapshot is always sufficient for normal operation.

When no model is explicitly set, MikMik scores available models by priority patterns to pick the best default. Well-known model prefixes (`claude-*`, `gpt-*`, `gemini-*`, etc.) are always routed to their canonical provider regardless of gateway entries in the remote cache.

### Overriding model metadata

Self-hosted endpoints (the `custom-openai` / Ollama / LM Studio / llama.cpp
providers) and model aliases that models.dev does not know can end up with the
wrong context window or max-output size — either because the alias is matched to
an unrelated catalog entry, or because there is no catalog entry at all. The
`modelOverrides` map lets you supply or correct that metadata. **User overrides
take precedence over the models.dev catalog and over the built-in defaults.**

Add it at the top level of `~/.config/mikmik/settings.json` (or inside the `config`
object), keyed by the fully-qualified `"provider/model"` id:

```json
{
  "modelOverrides": {
    "custom-openai/my-local-llm": {
      "contextWindow": 32768,
      "maxOutputTokens": 4096,
      "name": "My Local LLM",
      "releaseDate": "2026-01-01",
      "status": "beta"
    },
    "ollama/qwen3-coder-30b": {
      "contextWindow": 262144
    }
  }
}
```

**Fields** (all optional — an unset field keeps the catalog value):

| Field             | Type    | Description                                                |
|-------------------|---------|------------------------------------------------------------|
| `contextWindow`   | integer | Total context window size in tokens                        |
| `maxOutputTokens` | integer | Maximum tokens the model can emit in one response          |
| `name`            | string  | Human-readable display name shown in the model picker      |
| `releaseDate`     | string  | ISO 8601 date; drives newest-first ordering in the picker  |
| `status`          | string  | Lifecycle status (`active`, `beta`, `alpha`, `deprecated`) |

Field names accept both camelCase (`contextWindow`) and snake_case
(`context_window`). The key **must** contain a `/` — a bare model id is ignored,
because the registry is keyed by `provider/model`.

When the keyed model exists in the catalog, the override patches it in place.
When it does not (a self-hosted alias), MikMik materialises a synthetic entry
so the corrected values flow everywhere the metadata is read: the `/model`
picker, the token-usage warnings, and the auto-compact thresholds.
