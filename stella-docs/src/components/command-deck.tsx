"use client";

/**
 * The Command Deck — an interactive, live-streaming demo of Stella at work.
 *
 * It mirrors Stella's real multi-agent supervisor (the "Command Deck", the TUI
 * behind `stella chat` / `stella fleet`): a roster of workers that move through
 * the staged pipeline, cost metered against a budget, outcomes proven before a
 * run ends. Two scenarios share one grammar — a fan-out `fleet` and a single
 * `goal` run that drives to green — switchable by tab.
 *
 * Faithful, not fabricated: the states (queued / running / needs input / done /
 * failed), the pipeline stages (triage → plan → witness → execute → verify →
 * judge), the budget accounting, and the status colors are all Stella's own.
 * The numbers are illustrative of a run, not claims about a benchmark.
 *
 * Motion matches the docs' diagrams: calm, slow, no bounce. It loops on its own
 * and, under `prefers-reduced-motion`, freezes on a complete final frame so the
 * story still reads with zero animation.
 */

import { useEffect, useRef, useState } from "react";

type Status = "queued" | "running" | "needs-input" | "done" | "failed";

type Row = {
  label: string; // worker id, or pipeline stage name
  detail: string; // the task, or the sub-step
  status: Status;
  chip?: string; // current pipeline stage (fleet rows)
  spend?: number; // metered USD (fleet rows); omitted for goal
  note?: string; // trailing badge, e.g. "PR #981 · CI green"
};

type Frame = {
  rows: Row[];
  spent: number;
  wave: string;
  caption: string;
  hold: number; // ms to rest on this frame before the next
};

type Scenario = {
  key: string;
  tab: string;
  command: string;
  cap: number; // budget ceiling, USD
  showSpendColumn: boolean;
  frames: Frame[];
};

const STATUS_LABEL: Record<Status, string> = {
  queued: "queued",
  running: "running",
  "needs-input": "needs input",
  done: "done",
  failed: "failed",
};

/* ── Scenario: fan a batch of tasks out to a fleet ──────────────────────────
 * `stella --budget 2 fleet --plan release-prep.toml`
 * Workers coordinate over one tree by cooperative file claims; each is a full
 * pipeline run; spend meters into one aggregate cap. */
const FLEET: Scenario = {
  key: "fleet",
  tab: "stella fleet",
  command: "stella --budget 2 fleet --plan release-prep.toml",
  cap: 2,
  showSpendColumn: true,
  frames: [
    {
      spent: 0,
      wave: "wave 1 of 3 · dispatching",
      caption: "read the plan — 5 tasks, dependency-ordered into waves",
      hold: 1400,
      rows: [
        { label: "lead", detail: "release-prep · orchestrating", status: "running", chip: "triage", spend: 0.02 },
        { label: "parser-tests", detail: "add unit tests for the parser", status: "queued" },
        { label: "api-docs", detail: "document every fn in src/api/", status: "queued" },
        { label: "clippy", detail: "fix clippy in the store crate", status: "queued" },
        { label: "deps", detail: "bump deps to latest minor", status: "queued" },
      ],
    },
    {
      spent: 0.19,
      wave: "wave 1 of 3 · 3 running",
      caption: "claim-on-first-write keeps workers off each other's files",
      hold: 1900,
      rows: [
        { label: "lead", detail: "release-prep · orchestrating", status: "running", chip: "plan", spend: 0.06 },
        { label: "parser-tests", detail: "add unit tests for the parser", status: "running", chip: "execute", spend: 0.08 },
        { label: "api-docs", detail: "document every fn in src/api/", status: "running", chip: "witness", spend: 0.05 },
        { label: "clippy", detail: "fix clippy in the store crate", status: "queued" },
        { label: "deps", detail: "bump deps to latest minor", status: "queued" },
      ],
    },
    {
      spent: 0.58,
      wave: "wave 2 of 3 · 3 running",
      caption: "api-docs landed — committed on the shared branch, no merge-back",
      hold: 1900,
      rows: [
        { label: "lead", detail: "release-prep · orchestrating", status: "running", chip: "execute", spend: 0.14 },
        { label: "parser-tests", detail: "add unit tests for the parser", status: "running", chip: "verify", spend: 0.21 },
        { label: "api-docs", detail: "document every fn in src/api/", status: "done", spend: 0.18, note: "committed · 11 files" },
        { label: "clippy", detail: "fix clippy in the store crate", status: "running", chip: "execute", spend: 0.05 },
        { label: "deps", detail: "bump deps to latest minor", status: "queued" },
      ],
    },
    {
      spent: 1.02,
      wave: "wave 3 of 3 · 3 running",
      caption: "a judge verifies each result from evidence before it counts as done",
      hold: 2000,
      rows: [
        { label: "lead", detail: "release-prep · orchestrating", status: "running", chip: "verify", spend: 0.29 },
        { label: "parser-tests", detail: "add unit tests for the parser", status: "done", spend: 0.34, note: "committed · 6 tests green" },
        { label: "api-docs", detail: "document every fn in src/api/", status: "done", spend: 0.18, note: "committed · 11 files" },
        { label: "clippy", detail: "fix clippy in the store crate", status: "running", chip: "judge", spend: 0.16 },
        { label: "deps", detail: "bump deps to latest minor", status: "running", chip: "plan", spend: 0.05 },
      ],
    },
    {
      spent: 1.24,
      wave: "done · $0.76 left of $2.00",
      caption: "3 done · 1 paused for a decision · branches left in place to review",
      hold: 4200,
      rows: [
        { label: "lead", detail: "release-prep · orchestrating", status: "done", spend: 0.31, note: "judge: all goals met" },
        { label: "parser-tests", detail: "add unit tests for the parser", status: "done", spend: 0.34, note: "PR #982 · CI green" },
        { label: "api-docs", detail: "document every fn in src/api/", status: "done", spend: 0.18, note: "committed · 11 files" },
        { label: "clippy", detail: "fix clippy in the store crate", status: "done", spend: 0.29, note: "committed · clippy clean" },
        { label: "deps", detail: "bump deps to latest minor", status: "needs-input", spend: 0.12, note: "waiting: confirm a major bump" },
      ],
    },
  ],
};

/* ── Scenario: drive one goal to green ──────────────────────────────────────
 * `stella --budget 1 goal "make the auth suite pass"`
 * One agent walks the staged pipeline; an independent judge confirms the
 * definition of done from evidence — the run ends on proof, not a hunch. */
const GOAL: Scenario = {
  key: "goal",
  tab: "stella goal",
  command: 'stella --budget 1 goal "make the auth suite pass"',
  cap: 1,
  showSpendColumn: false,
  frames: [
    {
      spent: 0.03,
      wave: "done when: cargo test -p auth exits 0",
      caption: "a goal needs a checkable finish line — here, a green test suite",
      hold: 1300,
      rows: [
        { label: "triage", detail: "size the task, pick the route", status: "running" },
        { label: "plan", detail: "split the working context", status: "queued" },
        { label: "witness", detail: "reproduce the failure first", status: "queued" },
        { label: "execute", detail: "edit · run · re-check", status: "queued" },
        { label: "verify", detail: "flip the oracle", status: "queued" },
        { label: "judge", detail: "confirm from evidence", status: "queued" },
      ],
    },
    {
      spent: 0.11,
      wave: "witnessing the failure",
      caption: "it writes the failing test before touching a line of source",
      hold: 1700,
      rows: [
        { label: "triage", detail: "size the task, pick the route", status: "done" },
        { label: "plan", detail: "split the working context", status: "done" },
        { label: "witness", detail: "auth_test now fails for the right reason", status: "running" },
        { label: "execute", detail: "edit · run · re-check", status: "queued" },
        { label: "verify", detail: "flip the oracle", status: "queued" },
        { label: "judge", detail: "confirm from evidence", status: "queued" },
      ],
    },
    {
      spent: 0.24,
      wave: "in the step loop",
      caption: "edit, run, read the result, repeat — bounded, with the test as ground truth",
      hold: 1900,
      rows: [
        { label: "triage", detail: "size the task, pick the route", status: "done" },
        { label: "plan", detail: "split the working context", status: "done" },
        { label: "witness", detail: "auth_test reproduces the failure", status: "done" },
        { label: "execute", detail: "editing 3 files · cargo test", status: "running" },
        { label: "verify", detail: "flip the oracle", status: "queued" },
        { label: "judge", detail: "confirm from evidence", status: "queued" },
      ],
    },
    {
      spent: 0.31,
      wave: "cross-checking the finish",
      caption: "the oracle flips green — then a second, independent model judges it",
      hold: 1900,
      rows: [
        { label: "triage", detail: "size the task, pick the route", status: "done" },
        { label: "plan", detail: "split the working context", status: "done" },
        { label: "witness", detail: "auth_test reproduces the failure", status: "done" },
        { label: "execute", detail: "cargo test -p auth — 0 failures", status: "done" },
        { label: "verify", detail: "re-ran the suite clean twice", status: "done" },
        { label: "judge", detail: "cross-family review of the evidence", status: "running" },
      ],
    },
    {
      spent: 0.34,
      wave: "done · goal met · $0.66 left of $1.00",
      caption: "the run ends on proof a separate judge signed off — not the worker's say-so",
      hold: 4200,
      rows: [
        { label: "triage", detail: "size the task, pick the route", status: "done" },
        { label: "plan", detail: "split the working context", status: "done" },
        { label: "witness", detail: "auth_test reproduces the failure", status: "done" },
        { label: "execute", detail: "cargo test -p auth — 0 failures", status: "done" },
        { label: "verify", detail: "re-ran the suite clean twice", status: "done" },
        { label: "judge", detail: "goal met · verified from evidence", status: "done", note: "signed off" },
      ],
    },
  ],
};

const SCENARIOS: Scenario[] = [FLEET, GOAL];

/** Final frame with the command fully typed — the SSR / reduced-motion poster. */
function poster(s: Scenario) {
  return { typed: s.command.length, frame: s.frames.length - 1 };
}

function money(n: number) {
  return `$${n.toFixed(2)}`;
}

export function CommandDeck() {
  const [scenarioKey, setScenarioKey] = useState(SCENARIOS[0].key);
  const scenario = SCENARIOS.find((s) => s.key === scenarioKey) ?? SCENARIOS[0];

  // Initial state must match the server render → the complete poster frame.
  const [typed, setTyped] = useState(() => poster(scenario).typed);
  const [frameIdx, setFrameIdx] = useState(() => poster(scenario).frame);

  const timers = useRef<ReturnType<typeof setTimeout>[]>([]);

  useEffect(() => {
    const clear = () => {
      timers.current.forEach(clearTimeout);
      timers.current = [];
    };
    clear();

    const reduce =
      typeof window !== "undefined" &&
      window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;

    if (reduce) {
      // Freeze on the finished run — the whole story, no motion.
      const p = poster(scenario);
      setTyped(p.typed);
      setFrameIdx(p.frame);
      return clear;
    }

    const at = (ms: number, fn: () => void) => {
      timers.current.push(setTimeout(fn, ms));
    };

    let t = 0;
    const cycle = () => {
      // 1. Clear the roster and type the command.
      setFrameIdx(-1);
      setTyped(0);
      for (let i = 1; i <= scenario.command.length; i++) {
        at((t += 30), () => setTyped(i));
      }
      t += 450; // beat after the prompt lands
      // 2. Walk the frames, resting on each.
      scenario.frames.forEach((f, i) => {
        at(t, () => setFrameIdx(i));
        t += f.hold;
      });
      // 3. Loop.
      at(t, cycle);
    };
    cycle();

    return clear;
    // Re-run (and reset the loop) whenever the active scenario changes.
  }, [scenario]);

  const frame = frameIdx >= 0 ? scenario.frames[frameIdx] : null;
  const rows = frame?.rows ?? scenario.frames[0].rows.map((r) => ({ ...r, status: "queued" as Status, chip: undefined, spend: undefined, note: undefined }));
  const spent = frame?.spent ?? 0;
  const pct = Math.min(spent / scenario.cap, 1) * 100;
  const doneCount = rows.filter((r) => r.status === "done").length;

  return (
    <div className="deck-window overflow-hidden rounded-xl border border-fd-border bg-fd-card text-left">
      {/* Chrome: traffic lights + title + scenario tabs */}
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-fd-border px-4 py-2.5">
        <span className="flex items-center gap-1.5">
          <span className="size-2.5 rounded-full bg-fd-muted-foreground/30" />
          <span className="size-2.5 rounded-full bg-fd-muted-foreground/30" />
          <span className="size-2.5 rounded-full bg-fd-muted-foreground/30" />
        </span>
        <span className="deck-mono text-xs text-fd-muted-foreground">stella — command deck</span>
        <div className="ml-auto flex items-center gap-1" role="tablist" aria-label="Choose a demo">
          {SCENARIOS.map((s) => (
            <button
              key={s.key}
              type="button"
              role="tab"
              aria-selected={s.key === scenarioKey}
              data-active={s.key === scenarioKey}
              onClick={() => setScenarioKey(s.key)}
              className="deck-tab deck-mono rounded-md px-2.5 py-1 text-xs"
            >
              {s.tab}
            </button>
          ))}
        </div>
      </div>

      {/* Command line */}
      <div className="deck-mono border-b border-fd-border px-4 py-3 text-sm">
        <span className="deck-prompt">$ </span>
        <span className="text-fd-foreground">{scenario.command.slice(0, typed)}</span>
        {typed < scenario.command.length && <span className="lp-caret ml-0.5 align-baseline" aria-hidden />}
      </div>

      {/* Roster — decorative + looping, so it's spoken as one summary, not streamed. */}
      <div className="deck-mono px-2 py-2 text-sm sm:px-3" aria-hidden>
        {rows.map((r, i) => (
          <div
            key={r.label}
            className="deck-row flex items-center gap-3 rounded-lg px-2 py-1.5 transition-colors hover:bg-fd-accent/60"
            style={{ animationDelay: `${i * 55}ms` }}
          >
            <span className="deck-dot" data-status={r.status} aria-hidden />
            <span className="w-24 shrink-0 truncate font-medium text-fd-foreground sm:w-28">{r.label}</span>
            <span className="min-w-0 flex-1 truncate text-fd-muted-foreground">
              {r.note ? (
                <>
                  <span className="text-fd-foreground/80">{r.detail}</span>
                  <span className="mx-1.5 text-fd-border">·</span>
                  <span className="text-fd-muted-foreground">{r.note}</span>
                </>
              ) : (
                r.detail
              )}
            </span>
            {r.chip && (
              <span className="deck-stage hidden shrink-0 rounded px-1.5 py-0.5 text-[10px] sm:inline">{r.chip}</span>
            )}
            <span className="deck-status w-20 shrink-0 text-right text-xs" data-status={r.status}>
              {STATUS_LABEL[r.status]}
            </span>
            {scenario.showSpendColumn && (
              <span className="hidden w-12 shrink-0 text-right text-xs tabular-nums text-fd-muted-foreground sm:inline">
                {r.spend != null ? money(r.spend) : "—"}
              </span>
            )}
          </div>
        ))}
      </div>

      {/* Budget meter + narration */}
      <div className="border-t border-fd-border px-4 py-3">
        <div className="flex items-center gap-3">
          <span className="deck-mono text-[11px] uppercase tracking-wide text-fd-muted-foreground">budget</span>
          <div className="deck-meter h-1.5 flex-1">
            <div className="deck-meter-fill h-full" style={{ width: `${pct}%` }} data-over={spent > scenario.cap} />
          </div>
          <span className="deck-mono shrink-0 text-xs tabular-nums text-fd-foreground">
            {money(spent)} / {money(scenario.cap)}
          </span>
        </div>
        <div className="mt-2.5 flex flex-wrap items-center justify-between gap-2 text-xs">
          <span className="text-fd-muted-foreground">{frame?.caption ?? "ready"}</span>
          <span className="deck-mono shrink-0 text-fd-muted-foreground">{frame?.wave ?? ""}</span>
        </div>
      </div>

      {/* One quiet, static line for assistive tech in place of the looping roster. */}
      <p className="sr-only">
        {scenario.key === "fleet"
          ? `A live demo of ${scenario.command}: five worker agents run in parallel over one repository, each moving through Stella's staged pipeline while cost meters against a $${scenario.cap.toFixed(0)} budget. Four finish and one pauses for a decision, spending ${money(FLEET.frames[FLEET.frames.length - 1].spent)} total.`
          : `A live demo of ${scenario.command}: one agent drives the staged pipeline — triage, plan, witness, execute, verify — and an independent judge confirms the goal from evidence before the run ends, spending ${money(GOAL.frames[GOAL.frames.length - 1].spent)}.`}
        {" "}Currently showing: {doneCount} of {rows.length} rows complete.
      </p>
    </div>
  );
}

/**
 * The hero terminal — a lighter typed prompt that resolves to a proven result.
 * Same window language as the deck, but a single command and a two-line finish.
 */
const HERO_COMMAND = 'stella run "fix the failing test"';
const HERO_RESULT = [
  { text: "triage → plan → witness → execute → verify", tone: "muted" as const },
  { text: "✓ verified — src/parser.rs · 1 test now green", tone: "ok" as const },
];

export function HeroTerminal() {
  const [typed, setTyped] = useState(HERO_COMMAND.length);
  const [showResult, setShowResult] = useState(true);
  const timers = useRef<ReturnType<typeof setTimeout>[]>([]);

  useEffect(() => {
    const clear = () => {
      timers.current.forEach(clearTimeout);
      timers.current = [];
    };
    const reduce =
      typeof window !== "undefined" &&
      window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
    if (reduce) return clear;

    const at = (ms: number, fn: () => void) => timers.current.push(setTimeout(fn, ms));

    let t = 0;
    const cycle = () => {
      setShowResult(false);
      setTyped(0);
      for (let i = 1; i <= HERO_COMMAND.length; i++) at((t += 42), () => setTyped(i));
      t += 500;
      at(t, () => setShowResult(true));
      t += 5200;
      at(t, cycle);
    };
    clear();
    cycle();
    return clear;
  }, []);

  return (
    <div className="deck-window overflow-hidden rounded-xl border border-fd-border bg-fd-card text-left">
      <div className="flex items-center gap-1.5 border-b border-fd-border px-4 py-2.5">
        <span className="size-2.5 rounded-full bg-fd-muted-foreground/30" />
        <span className="size-2.5 rounded-full bg-fd-muted-foreground/30" />
        <span className="size-2.5 rounded-full bg-fd-muted-foreground/30" />
        <span className="deck-mono ml-2 text-xs text-fd-muted-foreground">zsh — stella</span>
      </div>
      <div className="deck-mono space-y-1.5 px-4 py-4 text-sm">
        <p>
          <span className="deck-prompt">$ </span>
          <span className="text-fd-foreground">export ANTHROPIC_API_KEY=…</span>
        </p>
        <p>
          <span className="deck-prompt">$ </span>
          <span className="text-fd-foreground">{HERO_COMMAND.slice(0, typed)}</span>
          {typed < HERO_COMMAND.length && <span className="lp-caret ml-0.5 align-baseline" aria-hidden />}
        </p>
        <div
          className="space-y-1 pt-1 transition-opacity duration-500"
          style={{ opacity: showResult ? 1 : 0 }}
          aria-hidden={!showResult}
        >
          {HERO_RESULT.map((line) => (
            <p
              key={line.text}
              className="pl-3.5 text-xs"
              style={{ color: line.tone === "ok" ? "var(--deck-ok)" : "var(--color-fd-muted-foreground)" }}
            >
              {line.text}
            </p>
          ))}
        </div>
      </div>
    </div>
  );
}
