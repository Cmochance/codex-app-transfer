import type { ReactNode } from "react";

const BASE = import.meta.env.BASE_URL;
const P = `${BASE}project/`;
const REPO = "https://github.com/Cmochance/codex-app-transfer";
const RELEASES = "https://github.com/Cmochance/codex-app-transfer/releases";
const VERSION = "v2.4.2";

const capabilities = [
  {
    n: "01",
    title: "Protocol translation",
    desc: "Translates Codex's Responses API (streaming + non-streaming) into Chat Completions, Gemini Native, Gemini CLI OAuth, Anthropic Messages, Grok Web, and Responses passthrough.",
  },
  {
    n: "02",
    title: "Provider & model mapping",
    desc: "Manage multiple providers side by side. Map OpenAI model slots (gpt-5.5 / gpt-5.4 / ...) to real upstream IDs like deepseek-v4-pro, kimi-k2.7, or gemini-3-pro.",
  },
  {
    n: "03",
    title: "Multi-turn tool conversations",
    desc: "Tool-call loops, previous_response_id replay, autocompact expansion, and thinking / reasoning_content injection — all aligned with the Responses protocol on every path.",
  },
  {
    n: "04",
    title: "apply_patch bridge + recovery",
    desc: "Bridges Responses custom_tool_call to chat function_call so the freeform edit-file tool works on third-party models, with a non-destructive middle layer that recovers malformed V4A patches.",
  },
  {
    n: "05",
    title: "Codex desktop enhancements",
    desc: "Injects 11 anime background themes, a Claude-style usage breakdown panel, and a draft stash — all runtime via CDP, never modifying the Codex binary.",
  },
  {
    n: "06",
    title: "Session history persistence",
    desc: "Two-layer LRU + SQLite store with content-addressed dedup (~97% body reduction). History survives restarts, never expires, and migrates legacy rows silently.",
  },
  {
    n: "07",
    title: "Config guardian",
    desc: "Snapshots and restores ~/.codex config before every apply, self-heals after force-kills, and keeps MCP OAuth credentials in a portable vault with per-entry recovery.",
  },
  {
    n: "08",
    title: "Real-account plugin unlock",
    desc: "Relays a real ChatGPT login so Codex shows Plugins natively, with a three-mode selector (off / synthetic / real) and automatic degradation when the account is unavailable.",
  },
];

const providers = [
  { name: "Kimi (Moonshot / For Coding)", multiturn: true, compact: true, tools: true, note: "Thinking 3-layer defense" },
  { name: "DeepSeek V4 (incl. Max)", multiturn: true, compact: true, tools: true, note: "xhigh -> real max effort" },
  { name: "Xiaomi MiMo", multiturn: true, compact: true, tools: true, note: "Native responses passthrough" },
  { name: "MiniMax M3 (1M) / M2.x", multiturn: true, compact: true, tools: true, note: "M3 context 1M" },
  { name: "OpenCode Go", multiturn: true, compact: true, tools: true, note: "Low-cost coding subscription" },
  { name: "WorkBuddy (Tencent CodeBuddy)", multiturn: "exp", compact: "exp", tools: "exp", note: "Multi-account pool + quota guard" },
  { name: "Google AI Studio (Gemini)", multiturn: true, compact: true, tools: true, note: "Gemini 3 /v1alpha + 2.x /v1beta" },
  { name: "Google Gemini CLI OAuth", multiturn: true, compact: true, tools: true, note: "Browser login, no API key" },
  { name: "Anthropic Messages", multiturn: true, compact: true, tools: true, note: "Claude-compatible" },
  { name: "Grok Web (SuperGrok / X+)", multiturn: true, compact: true, tools: true, note: "Experimental, personal use" },
  { name: "Google Antigravity", multiturn: true, compact: true, tools: true, note: "Native image_gen support" },
  { name: "Zhipu GLM / GLM Coding", multiturn: true, compact: true, tools: true, note: "GLM 5.2 is 1M context" },
  { name: "GLM (Z.ai / BigModel) OAuth", multiturn: true, compact: true, tools: true, note: "No API key, Coding Plan quota" },
  { name: "Aliyun Bailian (Qwen)", multiturn: true, compact: true, tools: true, note: "Qwen 3.6 Plus / Flash" },
];

const downloads = [
  { os: "Windows", arch: "x64", fmt: "NSIS Setup", file: ".exe", recommended: true },
  { os: "Windows", arch: "x64", fmt: "MSI", file: ".msi", recommended: false },
  { os: "macOS", arch: "Apple Silicon", fmt: "Disk Image", file: ".dmg", recommended: true },
  { os: "macOS", arch: "Intel x64", fmt: "Disk Image", file: ".dmg", recommended: false },
  { os: "Linux", arch: "x86_64", fmt: "Debian", file: ".deb", recommended: true },
  { os: "Linux", arch: "x86_64", fmt: "AppImage", file: ".AppImage", recommended: false },
];

function Check({ on }: { on: boolean | string }) {
  if (on === true) return <span className="font-mono text-orange-400">&#10003;</span>;
  if (on === "exp") return <span className="font-mono text-xs text-amber-400">exp</span>;
  return <span className="font-mono text-zinc-600">&mdash;</span>;
}

function Mono({ children }: { children: ReactNode }) {
  return <span className="font-mono">{children}</span>;
}

export default function App() {
  return (
    <div className="min-h-screen bg-zinc-950 text-zinc-100">
      {/* Nav */}
      <nav className="sticky top-0 z-50 border-b border-zinc-900 bg-zinc-950/80 backdrop-blur-md">
        <div className="mx-auto flex max-w-6xl items-center justify-between px-6 py-4">
          <div className="flex items-center gap-3">
            <img src={`${P}icon-nav.png`} alt="" className="h-7 w-7 rounded" />
            <span className="font-semibold tracking-tight">Codex App Transfer</span>
            <span className="rounded-full border border-zinc-800 px-2 py-0.5 font-mono text-xs text-zinc-400">
              {VERSION}
            </span>
          </div>
          <div className="flex items-center gap-6 font-mono text-sm text-zinc-400">
            <a href="#how" className="transition-colors hover:text-orange-400">how</a>
            <a href="#features" className="transition-colors hover:text-orange-400">features</a>
            <a href="#providers" className="transition-colors hover:text-orange-400">providers</a>
            <a href={REPO} className="hidden transition-colors hover:text-orange-400 sm:inline">github</a>
            <a
              href={RELEASES}
              className="rounded-md bg-orange-500 px-4 py-1.5 font-sans text-sm font-medium text-white transition-colors hover:bg-orange-600"
            >
              Download
            </a>
          </div>
        </div>
      </nav>

      {/* Hero */}
      <header className="hero-glow relative overflow-hidden border-b border-zinc-900">
        <div className="grid-bg absolute inset-0 opacity-60" />
        <div className="relative mx-auto max-w-6xl px-6 py-24 md:py-32">
          <p className="mb-6 font-mono text-sm tracking-widest text-orange-400">
            <Mono>// OPENAI CODEX &middot; DESKTOP GATEWAY</Mono>
          </p>
          <h1 className="max-w-3xl text-5xl font-semibold leading-[1.05] tracking-tight sm:text-6xl md:text-7xl">
            Run any model
            <br />
            inside <span className="text-orange-500">Codex</span>.
          </h1>
          <p className="mt-8 max-w-2xl text-lg leading-8 text-zinc-400">
            A lightweight desktop gateway that translates the Codex app's
            Responses API into Chat Completions, Gemini, Anthropic, and Grok
            protocols. Manage providers, model mappings, themes, and usage
            from one <Mono>Rust + Tauri</Mono> desktop UI.
          </p>
          <div className="mt-10 flex flex-wrap items-center gap-4">
            <a
              href={RELEASES}
              className="rounded-lg bg-orange-500 px-6 py-3 font-medium text-white shadow-lg shadow-orange-500/20 transition-colors hover:bg-orange-600"
            >
              Download {VERSION}
            </a>
            <a
              href={REPO}
              className="rounded-lg border border-zinc-700 px-6 py-3 font-medium text-zinc-200 transition-colors hover:border-zinc-500 hover:text-white"
            >
              View on GitHub &rarr;
            </a>
            <span className="font-mono text-sm text-zinc-500">
              <Mono>127.0.0.1:18080</Mono> &middot; zero-config
            </span>
          </div>

          {/* App window mockup */}
          <div className="app-frame mt-16">
            <div className="flex items-center gap-2 border-b border-zinc-800 bg-zinc-900/50 px-4 py-2.5">
              <span className="h-3 w-3 rounded-full bg-red-500/80" />
              <span className="h-3 w-3 rounded-full bg-yellow-500/80" />
              <span className="h-3 w-3 rounded-full bg-green-500/80" />
              <span className="ml-3 font-mono text-xs text-zinc-500">
                Codex App Transfer &mdash; Dashboard
              </span>
            </div>
            <img
              src={`${P}board.png`}
              alt="Codex App Transfer dashboard"
              className="w-full"
            />
          </div>
        </div>
      </header>

      {/* How it works */}
      <section id="how" className="border-b border-zinc-900 py-24">
        <div className="mx-auto max-w-6xl px-6">
          <p className="mb-3 font-mono text-sm tracking-widest text-orange-400">
            <Mono>// ARCHITECTURE</Mono>
          </p>
          <h2 className="text-4xl font-semibold tracking-tight">How it works</h2>
          <p className="mt-4 max-w-2xl text-zinc-400">
            The tool starts a local gateway. Codex sends its Responses API
            traffic there instead of OpenAI; the gateway translates each
            request into your chosen provider's protocol and forwards it
            upstream.
          </p>

          <div className="mt-14 flex flex-col items-stretch gap-4 md:flex-row md:items-center">
            <div className="flex-1 rounded-xl border border-zinc-800 bg-zinc-900/40 p-6">
              <p className="font-mono text-xs text-zinc-500">SOURCE</p>
              <p className="mt-2 text-lg font-semibold">Codex App</p>
              <p className="mt-1 text-sm text-zinc-500">
                OpenAI Responses API &middot; streaming
              </p>
            </div>
            <div className="flex items-center justify-center font-mono text-orange-400">
              <span className="hidden md:inline">&#8212;&#8212;&#9654;</span>
              <span className="md:hidden">&#9660;</span>
            </div>
            <div className="flex-[1.3] rounded-xl border border-orange-500/40 bg-orange-500/5 p-6">
              <p className="font-mono text-xs text-orange-400">GATEWAY</p>
              <p className="mt-2 text-lg font-semibold">Codex App Transfer</p>
              <p className="mt-1 font-mono text-sm text-zinc-400">
                127.0.0.1:18080 &middot; translate + forward
              </p>
            </div>
            <div className="flex items-center justify-center font-mono text-orange-400">
              <span className="hidden md:inline">&#8212;&#8212;&#9654;</span>
              <span className="md:hidden">&#9660;</span>
            </div>
            <div className="flex-1 rounded-xl border border-zinc-800 bg-zinc-900/40 p-6">
              <p className="font-mono text-xs text-zinc-500">UPSTREAM</p>
              <p className="mt-2 text-lg font-semibold">Your providers</p>
              <p className="mt-1 text-sm text-zinc-500">
                DeepSeek &middot; Gemini &middot; GLM &middot; Kimi ...
              </p>
            </div>
          </div>
        </div>
      </section>

      {/* Capabilities */}
      <section id="features" className="border-b border-zinc-900 py-24">
        <div className="mx-auto max-w-6xl px-6">
          <p className="mb-3 font-mono text-sm tracking-widest text-orange-400">
            <Mono>// CAPABILITIES</Mono>
          </p>
          <h2 className="text-4xl font-semibold tracking-tight">What it does</h2>
          <div className="mt-14 grid gap-x-12 gap-y-10 md:grid-cols-2">
            {capabilities.map((c) => (
              <div key={c.n} className="border-t border-zinc-800 pt-6">
                <div className="flex items-baseline gap-4">
                  <span className="font-mono text-sm text-orange-400">{c.n}</span>
                  <h3 className="text-xl font-semibold">{c.title}</h3>
                </div>
                <p className="mt-3 pl-9 leading-7 text-zinc-400">{c.desc}</p>
              </div>
            ))}
          </div>
        </div>
      </section>

      {/* Provider matrix */}
      <section id="providers" className="border-b border-zinc-900 py-24">
        <div className="mx-auto max-w-6xl px-6">
          <p className="mb-3 font-mono text-sm tracking-widest text-orange-400">
            <Mono>// COMPATIBILITY</Mono>
          </p>
          <h2 className="text-4xl font-semibold tracking-tight">
            Provider matrix
          </h2>
          <p className="mt-4 max-w-2xl text-zinc-400">
            Built-in presets with multi-turn history, autocompact, and
            tool-call repair verified across the providers below.
            <Mono className="text-amber-400"> exp</Mono> = smoke-tested only.
          </p>
          <div className="mt-10 overflow-x-auto">
            <table className="w-full border-collapse text-left text-sm">
              <thead>
                <tr className="border-b border-zinc-800 font-mono text-xs uppercase tracking-wider text-zinc-500">
                  <th className="py-3 pr-4 font-medium">Provider</th>
                  <th className="px-4 py-3 text-center font-medium">Multi-turn</th>
                  <th className="px-4 py-3 text-center font-medium">Autocompact</th>
                  <th className="px-4 py-3 text-center font-medium">Tool repair</th>
                  <th className="py-3 pl-4 font-medium">Notes</th>
                </tr>
              </thead>
              <tbody>
                {providers.map((p) => (
                  <tr
                    key={p.name}
                    className="border-b border-zinc-900 transition-colors hover:bg-zinc-900/40"
                  >
                    <td className="py-3 pr-4 font-medium text-zinc-200">
                      {p.name}
                    </td>
                    <td className="px-4 py-3 text-center">
                      <Check on={p.multiturn} />
                    </td>
                    <td className="px-4 py-3 text-center">
                      <Check on={p.compact} />
                    </td>
                    <td className="px-4 py-3 text-center">
                      <Check on={p.tools} />
                    </td>
                    <td className="py-3 pl-4 text-zinc-500">{p.note}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      </section>

      {/* Gallery */}
      <section className="border-b border-zinc-900 py-24">
        <div className="mx-auto max-w-6xl px-6">
          <p className="mb-3 font-mono text-sm tracking-widest text-orange-400">
            <Mono>// IN ACTION</Mono>
          </p>
          <h2 className="text-4xl font-semibold tracking-tight">
            See it in action
          </h2>

          {/* Main UI grid */}
          <div className="mt-12 grid gap-6 md:grid-cols-2">
            {[
              { src: "providers.png", label: "Providers — presets + model mapping" },
              { src: "logs.png", label: "Logs — live request stream" },
              { src: "codex-chat.png", label: "Codex app — real model in the picker" },
            ].map((g) => (
              <figure key={g.src} className="app-frame">
                <img src={`${P}${g.src}`} alt={g.label} className="w-full" loading="lazy" />
                <figcaption className="border-t border-zinc-800 px-4 py-2.5 font-mono text-xs text-zinc-500">
                  {g.label}
                </figcaption>
              </figure>
            ))}
          </div>

          {/* Codex enhancements */}
          <h3 className="mt-16 text-2xl font-semibold tracking-tight">
            Codex desktop enhancements
          </h3>
          <p className="mt-2 max-w-2xl text-zinc-400">
            Optional CDP-injected features that augment Codex without touching
            its binary — anime themes, a usage breakdown, and a draft stash.
          </p>
          <div className="mt-8 space-y-6">
            <figure className="app-frame mx-auto max-w-3xl">
              <img loading="lazy"
                src={`${P}usage-breakdown.jpg`}
                alt="Usage panel inside Codex"
                className="w-full"
              />
              <figcaption className="border-t border-zinc-800 px-4 py-2.5 font-mono text-xs text-zinc-500">
                Usage — quota, context, token rate, cache hit, by-source breakdown
              </figcaption>
            </figure>
            <div className="grid gap-6 md:grid-cols-2">
              <figure className="app-frame">
                <img loading="lazy"
                  src={`${P}stash-panel.png`}
                  alt="Stash panel"
                  className="w-full"
                />
                <figcaption className="border-t border-zinc-800 px-4 py-2.5 font-mono text-xs text-zinc-500">
                  Stash — park drafts + images, restore later
                </figcaption>
              </figure>
              <figure className="app-frame">
                <img loading="lazy"
                  src={`${P}stash-dropdown.png`}
                  alt="Stash quick restore"
                  className="w-full"
                />
                <figcaption className="border-t border-zinc-800 px-4 py-2.5 font-mono text-xs text-zinc-500">
                  Quick-restore dropdown
                </figcaption>
              </figure>
            </div>
          </div>

          {/* Themes */}
          <h3 className="mt-16 text-2xl font-semibold tracking-tight">
            Background themes
          </h3>
          <p className="mt-2 max-w-2xl text-zinc-400">
            11 built-in anime themes with per-image color grading, plus custom
            uploads with a 16:9 crop modal.
          </p>
          <div className="mt-8 grid gap-6 md:grid-cols-2">
            <figure className="app-frame">
                <img loading="lazy"
                  src={`${P}theme-changli.jpg`}
                  alt="Changli theme"
                  className="w-full"
                />
              <figcaption className="border-t border-zinc-800 px-4 py-2.5 font-mono text-xs text-zinc-500">
                Changli
              </figcaption>
            </figure>
            <figure className="app-frame">
                <img loading="lazy"
                  src={`${P}theme-azurlane.jpg`}
                  alt="Azur Lane theme"
                  className="w-full"
                />
              <figcaption className="border-t border-zinc-800 px-4 py-2.5 font-mono text-xs text-zinc-500">
                Azur Lane
              </figcaption>
            </figure>
          </div>
        </div>
      </section>

      {/* Tech stack + download */}
      <section className="border-b border-zinc-900 py-24">
        <div className="mx-auto max-w-6xl px-6">
          <div className="grid gap-16 md:grid-cols-2">
            <div>
              <p className="mb-3 font-mono text-sm tracking-widest text-orange-400">
                <Mono>// STACK</Mono>
              </p>
              <h2 className="text-4xl font-semibold tracking-tight">
                Built with Rust
              </h2>
              <p className="mt-4 leading-7 text-zinc-400">
                A native <Mono>Rust 1.85+</Mono> core paired with{" "}
                <Mono>Tauri 2.x</Mono> for the desktop shell. Single-instance
                locking, cross-process file locks, and a system tray across
                Windows, macOS, and Linux.
              </p>
              <div className="mt-8 flex flex-wrap gap-3 font-mono text-sm">
                {["Rust", "Tauri 2.x", "axum", "SQLite", "Vue 3", "CDP"].map(
                  (t) => (
                    <span
                      key={t}
                      className="rounded-md border border-zinc-800 bg-zinc-900/50 px-3 py-1.5 text-zinc-300"
                    >
                      {t}
                    </span>
                  ),
                )}
              </div>
              <div className="mt-8 rounded-lg border border-zinc-800 bg-zinc-900/40 p-5 font-mono text-sm text-zinc-400">
                <p className="text-zinc-500"># clone &amp; run</p>
                <p className="mt-2">
                  <span className="text-orange-400">git</span> clone {REPO}.git
                </p>
                <p className="mt-1">
                  <span className="text-orange-400">cargo</span> tauri dev
                </p>
              </div>
            </div>

            <div>
              <p className="mb-3 font-mono text-sm tracking-widest text-orange-400">
                <Mono>// DOWNLOAD</Mono>
              </p>
              <h2 className="text-4xl font-semibold tracking-tight">
                Get it now
              </h2>
              <p className="mt-4 text-zinc-400">
                Each binary ships with <Mono>.sha256</Mono> and{" "}
                <Mono>.sig</Mono> (RSA-3072) for verification.
              </p>
              <div className="mt-8 space-y-2.5">
                {downloads.map((d) => (
                  <a
                    key={`${d.os}-${d.arch}-${d.fmt}`}
                    href={RELEASES}
                    className="flex items-center justify-between rounded-lg border border-zinc-800 bg-zinc-900/40 px-5 py-3.5 transition-colors hover:border-orange-500/50 hover:bg-zinc-900"
                  >
                    <div className="flex items-center gap-3">
                      <span className="font-medium text-zinc-100">
                        {d.os}
                      </span>
                      <span className="font-mono text-xs text-zinc-500">
                        {d.arch} &middot; {d.fmt}
                      </span>
                    </div>
                    <div className="flex items-center gap-3">
                      {d.recommended && (
                        <span className="rounded-full bg-orange-500/15 px-2 py-0.5 font-mono text-xs text-orange-400">
                          recommended
                        </span>
                      )}
                      <span className="font-mono text-sm text-zinc-500">
                        {d.file}
                      </span>
                    </div>
                  </a>
                ))}
              </div>
            </div>
          </div>
        </div>
      </section>

      {/* Footer */}
      <footer className="py-16">
        <div className="mx-auto max-w-6xl px-6">
          <div className="flex flex-col items-start justify-between gap-8 md:flex-row md:items-center">
            <div>
              <div className="flex items-center gap-3">
                <img src={`${P}icon-nav.png`} alt="" className="h-8 w-8 rounded" />
                <span className="text-lg font-semibold">
                  Codex App Transfer
                </span>
              </div>
              <p className="mt-3 max-w-md text-sm text-zinc-500">
                An open-source local gateway for the OpenAI Codex app. Made
                with Rust, Tauri, and a love for anime themes.
              </p>
            </div>
            <div className="flex flex-col gap-2 font-mono text-sm text-zinc-400">
              <a href={REPO} className="transition-colors hover:text-orange-400">
                GitHub Repository
              </a>
              <a href={RELEASES} className="transition-colors hover:text-orange-400">
                Releases
              </a>
              <a href={`${REPO}/blob/main/CHANGELOG.md`} className="transition-colors hover:text-orange-400">
                Changelog
              </a>
              <a href={`${REPO}/blob/main/ACKNOWLEDGEMENTS.md`} className="transition-colors hover:text-orange-400">
                Acknowledgements
              </a>
            </div>
          </div>
          <div className="mt-12 border-t border-zinc-900 pt-6 font-mono text-xs text-zinc-600">
            &copy; 2026 Cmochance &middot; MIT License
          </div>
        </div>
      </footer>
    </div>
  );
}
