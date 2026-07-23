import Link from "next/link";
import {
  ArrowRight,
  Boxes,
  GitBranch,
  KeyRound,
  Layers,
  ListChecks,
  ShieldCheck,
  Terminal,
  Wrench,
  Gauge,
} from "lucide-react";
import { CommandDeck, HeroTerminal } from "@/components/command-deck";

/** The Stella star mark, inline so it inherits `currentColor` and flips with
 *  the theme (ink on paper, snow on black) — no per-mode asset needed. */
function StarMark({ className }: { className?: string }) {
  return (
    <svg viewBox="310 22 38 38" fill="currentColor" className={className} aria-hidden>
      <path d="M324.741 23.0512L327.351 38.0848L315.664 28.4041L311.238 35.0097L325.763 40.7043L311.579 46.0571L315.664 52.8906L327.465 43.096L324.741 58.3573L333.138 58.3573L330.415 43.096L342.102 52.8906L346.187 46.0571L332.117 40.7043L346.641 35.0097L342.216 28.4041L330.528 38.0848L333.138 23.0512L324.741 23.0512Z" />
    </svg>
  );
}

const FEATURES = [
  {
    icon: KeyRound,
    title: "Bring your own key",
    body: "No account, no sign-up. Stella auto-detects the provider from whichever API keys you already have and runs on your credentials — nothing is proxied through a hosted service.",
  },
  {
    icon: Boxes,
    title: "Model-agnostic",
    body: "Anthropic, OpenAI, Gemini, Vertex, Bedrock, xAI, DeepSeek, Z.ai, OpenRouter, and any OpenAI-compatible local server — one CLI, no rewrites when you switch.",
  },
  {
    icon: ListChecks,
    title: "Proves its work",
    body: "Goal mode doesn't stop on a hunch. A separate judge model verifies the definition of done from evidence before the loop ends — outcomes, not vibes.",
  },
  {
    icon: Wrench,
    title: "Real tools, gated",
    body: "Read, edit, grep, shell, web, CI, screenshots, issues, and your own script tools — each behind a per-tool permission model, with the shell off by default.",
  },
  {
    icon: Layers,
    title: "Durable sessions & fleets",
    body: "Pause, resume, and survive anything. Run one agent, or a fleet of workers over a shared task board with per-task worktree isolation and live PR/CI status.",
  },
  {
    icon: Gauge,
    title: "Local-first telemetry",
    body: "Token usage, cost, and per-step metering land in a local SQLite store on your disk. Community/default use sends nothing anywhere — inspect every run, share none of it.",
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
        <div className="relative mx-auto flex max-w-5xl flex-col items-center px-4 py-20 text-center sm:py-32">
          <StarMark className="mb-8 h-12 w-auto text-fd-foreground sm:h-14" />
          <span className="mb-6 inline-flex items-center gap-2 rounded-full border border-fd-border bg-fd-card px-3 py-1 text-xs font-medium text-fd-muted-foreground">
            <Terminal className="size-3.5" aria-hidden />
            A terminal coding agent that proves its work
          </span>
          <h1 className="max-w-3xl text-balance text-3xl font-semibold tracking-tight sm:text-6xl">
            Ship code from your terminal with{" "}
            <span className="lp-brand-text font-mono">stella</span>
          </h1>
          <p className="mt-6 max-w-2xl text-balance text-base text-fd-muted-foreground sm:text-lg">
            A fast, bring-your-own-key, model-agnostic coding agent. Point it at any
            provider, give it a goal, and let a verifier decide when the work is
            actually done.
          </p>

          <div className="mt-9 w-full max-w-xl">
            <HeroTerminal />
          </div>

          <div className="mt-8 flex w-full max-w-xs flex-col items-stretch gap-3 sm:max-w-none sm:flex-row sm:items-center sm:justify-center">
            <Link
              href="/docs"
              className="lp-cta inline-flex items-center justify-center gap-2 rounded-lg px-5 py-2.5 text-sm font-semibold transition-colors"
            >
              Read the docs
              <ArrowRight className="size-4" aria-hidden />
            </Link>
            <Link
              href="/docs/getting-started/installation"
              className="inline-flex items-center justify-center gap-2 rounded-lg border border-fd-border bg-fd-card px-5 py-2.5 text-sm font-semibold text-fd-foreground transition-colors hover:bg-fd-accent"
            >
              Install Stella
            </Link>
          </div>
        </div>
      </section>

      {/* Features */}
      <section className="mx-auto w-full max-w-6xl px-4 py-16 sm:py-20">
        <div className="grid gap-px overflow-hidden rounded-xl border border-fd-border bg-fd-border sm:grid-cols-2 lg:grid-cols-3">
          {FEATURES.map(({ icon: Icon, title, body }) => (
            <div key={title} className="bg-fd-background p-6">
              <div className="mb-4 inline-flex size-10 items-center justify-center rounded-lg border border-fd-border bg-fd-card">
                <Icon className="size-5 text-fd-foreground" aria-hidden />
              </div>
              <h3 className="text-base font-semibold">{title}</h3>
              <p className="mt-2 text-sm leading-relaxed text-fd-muted-foreground">
                {body}
              </p>
            </div>
          ))}
        </div>
      </section>

      {/* Command Deck — the signature: watch a run actually happen */}
      <section className="border-t border-fd-border bg-fd-muted/40">
        <div className="mx-auto w-full max-w-5xl px-4 py-16 sm:py-20">
          <div className="mb-8 flex flex-col items-start gap-3 sm:mb-10">
            <span className="inline-flex items-center gap-2 rounded-full border border-fd-border bg-fd-background px-3 py-1 text-xs font-medium text-fd-muted-foreground">
              <Gauge className="size-3.5" aria-hidden />
              The command deck, live
            </span>
            <h2 className="max-w-2xl text-balance text-2xl font-semibold tracking-tight sm:text-3xl">
              Don&apos;t take our word for it. Watch the work.
            </h2>
            <p className="max-w-2xl text-sm leading-relaxed text-fd-muted-foreground sm:text-base">
              This is Stella&apos;s real supervisor: a roster of agents moving through the
              staged pipeline, every dollar metered against a budget, every result
              proven before it counts as done. Switch between a{" "}
              <span className="font-medium text-fd-foreground">fleet</span> fanning tasks
              out in parallel and a single{" "}
              <span className="font-medium text-fd-foreground">goal</span> run driving to
              green.
            </p>
          </div>
          <CommandDeck />
          <p className="mt-4 text-center text-xs text-fd-muted-foreground">
            An illustrative run — the states, stages, and budget accounting are Stella&apos;s
            own.
          </p>
        </div>
      </section>

      {/* Providers */}
      <section className="border-y border-fd-border bg-fd-muted/40">
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
              <Link
                href="/docs/configuration/settings"
                className="rounded bg-fd-muted px-1.5 py-0.5 font-mono text-xs text-fd-foreground underline-offset-2 hover:underline"
              >
                settings.json
              </Link>{" "}
              — no provider-specific environment variables required.
            </p>
          </div>
        </div>
      </section>

      {/* Closing split */}
      <section className="mx-auto w-full max-w-6xl px-4 py-16 sm:py-20">
        <div className="mb-8 max-w-2xl">
          <h2 className="text-2xl font-semibold tracking-tight sm:text-3xl">
            Built to be believed
          </h2>
          <p className="mt-3 text-sm leading-relaxed text-fd-muted-foreground sm:text-base">
            Stella runs a real pipeline — triage, recall, plan, execute, verify, judge —
            so a run ends on proof, not the worker&apos;s own say-so. Scale it from one
            session to a fleet, and extend it to fit your workflow.
          </p>
        </div>
        <div className="grid gap-6 md:grid-cols-3">
          <SplitCard
            icon={ListChecks}
            title="Goal mode"
            body="Give Stella an objective and it drives to green — editing, running, and re-checking until an independent judge confirms the goal from evidence."
            href="/docs/agent-modes/goal-mode"
            cta="How goal mode works"
          />
          <SplitCard
            icon={GitBranch}
            title="Multi-agent fleets"
            body="Point many workers at one task board. Cooperative claims on a shared tree or per-task worktree isolation, with pause/resume and live PR & CI status."
            href="/docs/agent-fleets"
            cta="Run a fleet"
          />
          <SplitCard
            icon={Wrench}
            title="Extend it"
            body="Add MCP servers, custom script tools, skills, and lifecycle hooks. Stella meets your workflow instead of replacing it."
            href="/docs/agent-tools/custom-tools"
            cta="Add your own tools"
          />
        </div>
      </section>

      <footer className="border-t border-fd-border">
        <div className="mx-auto flex w-full max-w-6xl flex-col items-center justify-between gap-4 px-4 py-8 text-sm text-fd-muted-foreground sm:flex-row">
          <span className="inline-flex items-center gap-2">
            <StarMark className="h-4 w-auto text-fd-muted-foreground" />
            <span className="font-mono">stella</span>
          </span>
          <div className="flex items-center gap-5">
            <Link href="/docs" className="hover:text-fd-foreground">
              Docs
            </Link>
            <Link href="/docs/getting-started/installation" className="hover:text-fd-foreground">
              Install
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
        <Icon className="size-5 text-fd-foreground" aria-hidden />
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
