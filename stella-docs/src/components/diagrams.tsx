/**
 * Animated inline-SVG diagrams for the docs — "a little razzle".
 *
 * Ground rules, in order of importance:
 * - Theme-aware: every color is a Fumadocs/brand token (`--color-fd-*`,
 *   `--stella-*`) or `currentColor`, so light and dark both read.
 * - Calm: slow (5–9 s) low-contrast motion — flowing dashes and soft pulses,
 *   never bounces or confetti. Decoration, not distraction.
 * - Accessible: every diagram carries a `<title>` + `role="img"`, and a
 *   `prefers-reduced-motion` media query freezes all animation.
 * - Server-safe: pure SVG + CSS keyframes, no client JS.
 *
 * The `sdg-` class prefix (stella diagram) namespaces the shared stylesheet
 * so nothing leaks into page styles.
 */

const STYLE = `
.sdg { width: 100%; height: auto; display: block; margin: 1.5rem 0; }
.sdg text { font-family: var(--font-sans, ui-sans-serif, system-ui, sans-serif); }
.sdg .sdg-box { fill: var(--color-fd-card); stroke: var(--color-fd-border); stroke-width: 1.25; }
.sdg .sdg-box-accent { fill: color-mix(in oklab, var(--stella-azure) 12%, var(--color-fd-card)); stroke: var(--stella-azure); stroke-width: 1.25; }
.sdg .sdg-label { fill: var(--color-fd-foreground); font-size: 13px; font-weight: 600; }
.sdg .sdg-sub { fill: var(--color-fd-muted-foreground); font-size: 10.5px; font-weight: 400; }
.sdg .sdg-wire { stroke: var(--color-fd-border); stroke-width: 1.25; fill: none; }
.sdg .sdg-flow { stroke: var(--stella-azure); stroke-width: 1.5; fill: none; opacity: 0.9;
  stroke-dasharray: 6 10; animation: sdg-dash 7s linear infinite; }
.sdg .sdg-flow-slow { animation-duration: 9s; }
.sdg .sdg-pulse { animation: sdg-pulse 5s ease-in-out infinite; transform-origin: center; transform-box: fill-box; }
.sdg .sdg-dot { fill: var(--stella-azure); }
.sdg .sdg-check { stroke: var(--stella-azure); stroke-width: 2; fill: none; stroke-linecap: round;
  stroke-dasharray: 14; stroke-dashoffset: 14; animation: sdg-draw 6s ease-out infinite; }
@keyframes sdg-dash { to { stroke-dashoffset: -64; } }
@keyframes sdg-pulse { 0%, 100% { opacity: 0.55; } 50% { opacity: 1; } }
@keyframes sdg-draw { 0%, 55% { stroke-dashoffset: 14; } 70%, 90% { stroke-dashoffset: 0; opacity: 1; } 100% { stroke-dashoffset: 0; opacity: 0; } }
@media (prefers-reduced-motion: reduce) {
  .sdg .sdg-flow, .sdg .sdg-flow-slow { animation: none; stroke-dasharray: none; }
  .sdg .sdg-pulse { animation: none; opacity: 0.9; }
  .sdg .sdg-check { animation: none; stroke-dashoffset: 0; }
  .sdg animateMotion { display: none; }
}
`;

function Defs() {
  return (
    <defs>
      <marker
        id="sdg-arrow"
        viewBox="0 0 8 8"
        refX="7"
        refY="4"
        markerWidth="7"
        markerHeight="7"
        orient="auto-start-reverse"
      >
        <path d="M0.5 0.8 L7.2 4 L0.5 7.2" fill="none" stroke="var(--stella-azure)" strokeWidth="1.4" strokeLinecap="round" />
      </marker>
    </defs>
  );
}

/** Landing page: you → stella → your provider, telemetry staying local. */
export function HeroFlowDiagram() {
  return (
    <svg className="sdg" viewBox="0 0 720 190" role="img" aria-label="Your prompt flows through stella to the provider you chose; telemetry stays on your machine.">
      <title>How Stella fits together</title>
      <style>{STYLE}</style>
      <Defs />
      {/* you */}
      <rect className="sdg-box" x="20" y="52" width="120" height="56" rx="10" />
      <text className="sdg-label" x="80" y="76" textAnchor="middle">you</text>
      <text className="sdg-sub" x="80" y="93" textAnchor="middle">a prompt, a goal</text>
      {/* stella */}
      <rect className="sdg-box-accent sdg-pulse" x="280" y="40" width="160" height="80" rx="12" />
      <text className="sdg-label" x="360" y="72" textAnchor="middle">stella</text>
      <text className="sdg-sub" x="360" y="90" textAnchor="middle">tools · pipeline · judge</text>
      {/* provider */}
      <rect className="sdg-box" x="580" y="52" width="120" height="56" rx="10" />
      <text className="sdg-label" x="640" y="76" textAnchor="middle">provider</text>
      <text className="sdg-sub" x="640" y="93" textAnchor="middle">your key, direct</text>
      {/* wires */}
      <path className="sdg-wire" d="M140 80 H278" />
      <path className="sdg-flow" d="M140 80 H278" markerEnd="url(#sdg-arrow)" />
      <path className="sdg-wire" d="M440 80 H578" />
      <path className="sdg-flow" d="M440 80 H578" markerEnd="url(#sdg-arrow)" />
      {/* local telemetry */}
      <path className="sdg-wire" d="M360 120 V150" />
      <rect className="sdg-box" x="285" y="150" width="150" height="32" rx="8" />
      <text className="sdg-sub" x="360" y="170" textAnchor="middle">.stella/ — telemetry stays here</text>
    </svg>
  );
}

/** Inference pipeline: the staged flow with the revise return edge. */
export function PipelineFlowDiagram() {
  const stages: [string, string][] = [
    ["triage", "route it"],
    ["plan", "split context"],
    ["witness", "failing test"],
    ["execute", "step loop"],
    ["verify", "flip oracle"],
    ["judge", "cross-family"],
  ];
  return (
    <svg className="sdg" viewBox="0 0 720 150" role="img" aria-label="The staged pipeline: triage, plan, witness, execute, verify, judge — with a revise loop back into execute.">
      <title>The staged inference pipeline</title>
      <style>{STYLE}</style>
      <Defs />
      {stages.map(([name, sub], i) => {
        const x = 16 + i * 118;
        const accent = name === "verify" || name === "judge";
        return (
          <g key={name}>
            <rect className={accent ? "sdg-box-accent" : "sdg-box"} x={x} y={44} width={100} height={52} rx={10} />
            <text className="sdg-label" x={x + 50} y={67} textAnchor="middle">{name}</text>
            <text className="sdg-sub" x={x + 50} y={84} textAnchor="middle">{sub}</text>
            {i < stages.length - 1 && (
              <path className="sdg-wire" d={`M${x + 100} 70 H${x + 116}`} />
            )}
          </g>
        );
      })}
      {/* one continuous flow line under the chain for the traveling dashes */}
      <path className="sdg-flow" d="M116 70 H134 M234 70 H252 M352 70 H370 M470 70 H488 M588 70 H606" />
      {/* the verify check, drawing itself periodically */}
      <path className="sdg-check" d="M509 66 l6 7 l11 -13" />
      {/* revise: judge back to execute */}
      <path className="sdg-wire" d="M672 96 C672 132 420 132 420 98" />
      <path className="sdg-flow sdg-flow-slow" d="M672 96 C672 132 420 132 420 98" markerEnd="url(#sdg-arrow)" />
      <text className="sdg-sub" x="546" y="140" textAnchor="middle">revise — bounded, with evidence</text>
    </svg>
  );
}

/** Context engine: the recall → work → cite/reflect loop around the stores. */
export function RecallLoopDiagram() {
  return (
    <svg className="sdg" viewBox="0 0 720 200" role="img" aria-label="Memories and the code graph feed the recall block, the model works, citations and reflections feed back into the stores.">
      <title>The context loop</title>
      <style>{STYLE}</style>
      <Defs />
      {/* stores */}
      <rect className="sdg-box" x="24" y="40" width="168" height="46" rx="10" />
      <text className="sdg-label" x="108" y="59" textAnchor="middle">memories · rules</text>
      <text className="sdg-sub" x="108" y="76" textAnchor="middle">.stella/memories · rules</text>
      <rect className="sdg-box" x="24" y="112" width="168" height="46" rx="10" />
      <text className="sdg-label" x="108" y="131" textAnchor="middle">code graph</text>
      <text className="sdg-sub" x="108" y="148" textAnchor="middle">tree-sitter index</text>
      {/* recall block */}
      <rect className="sdg-box-accent sdg-pulse" x="280" y="76" width="160" height="48" rx="10" />
      <text className="sdg-label" x="360" y="95" textAnchor="middle">recall block</text>
      <text className="sdg-sub" x="360" y="112" textAnchor="middle">5 frames · ~1,200 tokens</text>
      {/* model */}
      <rect className="sdg-box" x="528" y="76" width="168" height="48" rx="10" />
      <text className="sdg-label" x="612" y="95" textAnchor="middle">the model turn</text>
      <text className="sdg-sub" x="612" y="112" textAnchor="middle">cached prefix + recall</text>
      {/* forward wires */}
      <path className="sdg-wire" d="M192 63 C240 63 240 88 278 92" />
      <path className="sdg-flow" d="M192 63 C240 63 240 88 278 92" markerEnd="url(#sdg-arrow)" />
      <path className="sdg-wire" d="M192 135 C240 135 240 112 278 108" />
      <path className="sdg-flow" d="M192 135 C240 135 240 112 278 108" markerEnd="url(#sdg-arrow)" />
      <path className="sdg-wire" d="M440 100 H526" />
      <path className="sdg-flow" d="M440 100 H526" markerEnd="url(#sdg-arrow)" />
      {/* feedback: cite_memory / reflections back to the stores */}
      <path className="sdg-wire" d="M612 124 C612 182 108 182 108 160" />
      <path className="sdg-flow sdg-flow-slow" d="M612 124 C612 182 108 182 108 160" markerEnd="url(#sdg-arrow)" />
      <text className="sdg-sub" x="360" y="176" textAnchor="middle">cite_memory · reflections · episodes — memory that earns its place</text>
    </svg>
  );
}

/** Fleet: one base commit fanning out to worktree lanes, converging on review. */
export function FleetFanoutDiagram() {
  const lanes = [
    { y: 40, label: "fleet/t1" },
    { y: 90, label: "fleet/t2" },
    { y: 140, label: "fleet/t3" },
  ];
  return (
    <svg className="sdg" viewBox="0 0 720 180" role="img" aria-label="A pinned base commit fans out to isolated worktree branches; finished branches converge on your review.">
      <title>Fleet fan-out over git worktrees</title>
      <style>{STYLE}</style>
      <Defs />
      {/* base */}
      <circle className="sdg-dot sdg-pulse" cx="60" cy="90" r="7" />
      <text className="sdg-label" x="60" y="66" textAnchor="middle">base</text>
      <text className="sdg-sub" x="60" y="118" textAnchor="middle">pinned SHA</text>
      {lanes.map(({ y, label }) => (
        <g key={label}>
          <path className="sdg-wire" d={`M68 90 C130 90 130 ${y} 190 ${y} H520`} />
          <path className="sdg-flow" d={`M68 90 C130 90 130 ${y} 190 ${y} H520`} />
          <rect className="sdg-box" x="240" y={y - 15} width="180" height="30" rx="8" />
          <text className="sdg-sub" x="330" y={y + 4} textAnchor="middle">{label} — its own worktree</text>
          <path className="sdg-wire" d={`M520 ${y} C580 ${y} 580 90 636 90`} />
          <path className="sdg-flow sdg-flow-slow" d={`M520 ${y} C580 ${y} 580 90 636 90`} markerEnd="url(#sdg-arrow)" />
        </g>
      ))}
      {/* review */}
      <circle className="sdg-dot sdg-pulse" cx="648" cy="90" r="7" />
      <text className="sdg-label" x="648" y="66" textAnchor="middle">review</text>
      <text className="sdg-sub" x="648" y="118" textAnchor="middle">merge on your terms</text>
    </svg>
  );
}
