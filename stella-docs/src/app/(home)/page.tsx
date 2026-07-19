import Link from "next/link";
import {
  ArrowRight,
  Boxes,
  GitBranch,
  KeyRound,
  ListChecks,
  Plug,
  ShieldCheck,
  Terminal,
  Wrench,
  Gauge,
} from "lucide-react";

const FEATURES = [
  {
    icon: KeyRound,
    title: "Bring your own key",
    body: "No account, no sign-up. Stella auto-detects the provider from whichever API keys you have set and runs on your credentials.",
  },
  {
    icon: Boxes,
    title: "Model-agnostic",
    body: "Anthropic, OpenAI, Gemini, Vertex, Bedrock, xAI, DeepSeek, Z.ai, OpenRouter, and any OpenAI-compatible local server — one CLI.",
  },
  {
    icon: ListChecks,
    title: "Verified outcomes",
    body: "Goal mode doesn't stop on a hunch. A separate judge model verifies the definition of done from evidence before the loop ends.",
  },
  {
    icon: Wrench,
    title: "Real tools",
    body: "Read, edit, glob, grep, shell, CI, screenshots, issues, and developer-defined script tools — with a per-tool permission model.",
  },
  {
    icon: Plug,
    title: "MCP native",
    body: "Connect Model Context Protocol servers from .stella/mcp.toml to bring more tools into the agent at session start.",
  },
  {
    icon: Gauge,
    title: "Local telemetry",
    body: "Token usage, cost, and step metering land in a local SQLite store on your disk — inspect every run, share nothing.",
  },
];

const PROVIDERS = [
  "Anthropic",
  "OpenAI",
  "Google Gemini",
  "Vertex AI",
  "Amazon Bedrock",
  "xAI",
  "DeepSeek",
  "Z.ai",
  "OpenRouter",
  "Local / OpenAI-compatible",
];

export default function HomePage() {
  return (
    <main className="flex flex-1 flex-col">
      {/* Hero */}
      <section className="relative overflow-hidden border-b border-fd-border">
        <div className="lp-hero-grid pointer-events-none absolute inset-0" aria-hidden />
        <div className="lp-hero-glow pointer-events-none absolute inset-0" aria-hidden />
        <div className="relative mx-auto flex max-w-5xl flex-col items-center px-4 py-24 text-center sm:py-32">
          {/* eslint-disable-next-line @next/next/no-img-element */}
          <img
            src="/brand/mark.svg"
            alt=""
            aria-hidden
            className="mb-8 h-14 w-auto drop-shadow-[0_0_28px_rgba(63,224,255,0.35)]"
          />
          <span className="mb-6 inline-flex items-center gap-2 rounded-full border border-fd-border bg-fd-card px-3 py-1 text-xs font-medium text-fd-muted-foreground">
            <Terminal className="size-3.5 text-[var(--stella-azure)]" aria-hidden />
            A terminal coding agent
          </span>
          <h1 className="max-w-3xl text-balance text-4xl font-semibold tracking-tight sm:text-6xl">
            Ship code from your terminal with{" "}
            <span className="lp-brand-text font-mono">stella</span>
          </h1>
          <p className="mt-6 max-w-2xl text-balance text-lg text-fd-muted-foreground">
            A fast, bring-your-own-key, model-agnostic coding agent. Point it at any
            provider, give it a goal, and let a verifier decide when the work is
            actually done.
          </p>

          <div className="mt-9 w-full max-w-xl">
            <TerminalCard />
          </div>

          <div className="mt-8 flex flex-wrap items-center justify-center gap-3">
            <Link
              href="/docs"
              className="lp-cta inline-flex items-center gap-2 rounded-lg px-5 py-2.5 text-sm font-semibold transition-[filter]"
            >
              Read the docs
              <ArrowRight className="size-4" aria-hidden />
            </Link>
            <Link
              href="/docs/getting-started/installation"
              className="inline-flex items-center gap-2 rounded-lg border border-fd-border bg-fd-card px-5 py-2.5 text-sm font-semibold text-fd-foreground transition-colors hover:bg-fd-accent"
            >
              Install
            </Link>
          </div>
        </div>
      </section>

      {/* Features */}
      <section className="mx-auto w-full max-w-6xl px-4 py-20">
        <div className="grid gap-px overflow-hidden rounded-xl border border-fd-border bg-fd-border sm:grid-cols-2 lg:grid-cols-3">
          {FEATURES.map(({ icon: Icon, title, body }) => (
            <div key={title} className="bg-fd-background p-6">
              <div className="mb-4 inline-flex size-10 items-center justify-center rounded-lg border border-fd-border bg-fd-card">
                <Icon className="size-5 text-[var(--stella-azure)]" aria-hidden />
              </div>
              <h3 className="text-base font-semibold">{title}</h3>
              <p className="mt-2 text-sm leading-relaxed text-fd-muted-foreground">
                {body}
              </p>
            </div>
          ))}
        </div>
      </section>

      {/* Providers */}
      <section className="border-y border-fd-border bg-fd-card/40">
        <div className="mx-auto w-full max-w-6xl px-4 py-16">
          <div className="flex flex-col items-center text-center">
            <h2 className="text-sm font-semibold uppercase tracking-wide text-fd-muted-foreground">
              Works with your provider
            </h2>
            <div className="mt-6 flex flex-wrap justify-center gap-2">
              {PROVIDERS.map((p) => (
                <span
                  key={p}
                  className="rounded-full border border-fd-border bg-fd-background px-3.5 py-1.5 text-sm text-fd-foreground"
                >
                  {p}
                </span>
              ))}
            </div>
            <p className="mt-6 max-w-2xl text-sm text-fd-muted-foreground">
              Override any provider&apos;s base URL, key, or model in{" "}
              <code className="rounded bg-fd-muted px-1.5 py-0.5 text-xs">
                settings.json
              </code>{" "}
              — no provider-specific environment variables required.
            </p>
          </div>
        </div>
      </section>

      {/* Closing split */}
      <section className="mx-auto grid w-full max-w-6xl gap-6 px-4 py-20 md:grid-cols-3">
        <SplitCard
          icon={GitBranch}
          title="Goal mode"
          body="Give Stella an objective and it drives to green — editing, running, and re-checking until a judge confirms the goal from evidence."
          href="/docs/agent-modes/goal-mode"
          cta="How goal mode works"
        />
        <SplitCard
          icon={ShieldCheck}
          title="Credentials & scopes"
          body="A clear credential chain and a three-tier settings hierarchy — project, org-managed, and user — so teams share defaults safely."
          href="/docs/configuration/settings"
          cta="Configure settings"
        />
        <SplitCard
          icon={Wrench}
          title="Extend it"
          body="Add MCP servers, custom script tools, skills, and lifecycle hooks. Stella meets your workflow instead of replacing it."
          href="/docs/agent-tools/custom-tools"
          cta="Add your own tools"
        />
      </section>

      <footer className="border-t border-fd-border">
        <div className="mx-auto flex w-full max-w-6xl flex-col items-center justify-between gap-4 px-4 py-8 text-sm text-fd-muted-foreground sm:flex-row">
          <span className="inline-flex items-center gap-2">
            {/* eslint-disable-next-line @next/next/no-img-element */}
            <img src="/brand/mark-flat.svg" alt="" aria-hidden className="h-4 w-auto opacity-80" />
            <span className="font-mono">stella</span>
          </span>
          <div className="flex items-center gap-5">
            <Link href="/docs" className="hover:text-fd-foreground">
              Docs
            </Link>
            <a
              href="https://github.com/macanderson/stella"
              className="hover:text-fd-foreground"
            >
              GitHub
            </a>
          </div>
        </div>
      </footer>
    </main>
  );
}

function TerminalCard() {
  return (
    <div className="overflow-hidden rounded-xl border border-fd-border bg-fd-card text-left shadow-sm">
      <div className="flex items-center gap-1.5 border-b border-fd-border px-4 py-2.5">
        <span className="size-2.5 rounded-full bg-fd-muted-foreground/30" />
        <span className="size-2.5 rounded-full bg-fd-muted-foreground/30" />
        <span className="size-2.5 rounded-full bg-fd-muted-foreground/30" />
        <span className="ml-2 text-xs text-fd-muted-foreground">zsh — stella</span>
      </div>
      <div className="lp-terminal space-y-1 px-4 py-4 text-sm">
        <p>
          <span className="lp-prompt">$ </span>
          <span className="text-fd-foreground">export ANTHROPIC_API_KEY=…</span>
        </p>
        <p>
          <span className="lp-prompt">$ </span>
          <span className="text-fd-foreground">
            stella run &quot;fix the failing test&quot;
          </span>
          <span className="lp-caret ml-1 align-baseline" aria-hidden />
        </p>
      </div>
    </div>
  );
}

function SplitCard({
  icon: Icon,
  title,
  body,
  href,
  cta,
}: {
  icon: typeof GitBranch;
  title: string;
  body: string;
  href: string;
  cta: string;
}) {
  return (
    <div className="flex flex-col rounded-xl border border-fd-border bg-fd-card p-6">
      <div className="mb-4 inline-flex size-10 items-center justify-center rounded-lg border border-fd-border bg-fd-background">
        <Icon className="size-5 text-[var(--stella-azure)]" aria-hidden />
      </div>
      <h3 className="text-lg font-semibold">{title}</h3>
      <p className="mt-2 flex-1 text-sm leading-relaxed text-fd-muted-foreground">
        {body}
      </p>
      <Link
        href={href}
        className="mt-4 inline-flex items-center gap-1.5 text-sm font-medium text-fd-primary hover:underline"
      >
        {cta}
        <ArrowRight className="size-4" aria-hidden />
      </Link>
    </div>
  );
}
