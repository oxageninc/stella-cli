"use client";

/**
 * The per-page footer every docs page renders: share menu, the GitHub repo
 * button, the Sponsor button, and the free-and-open-source license line.
 *
 * Taste rules: quiet chrome (borders + muted text, no gradients), the
 * GitHub button is GitHub-themed (octocat + repo slug on a near-black tile,
 * inverted in dark mode), share targets are plain text rows (no faux brand
 * icons), and everything keys off the Fumadocs tokens so both themes read.
 */

import { useEffect, useRef, useState } from "react";
import { Check, Copy, Heart, Share2 } from "lucide-react";

const REPO_URL = "https://github.com/macanderson/stella";
const SPONSOR_URL = "https://github.com/sponsors/macanderson";

function GitHubMark({ className }: { className?: string }) {
  return (
    <svg role="img" viewBox="0 0 24 24" fill="currentColor" className={className} aria-hidden>
      <path d="M12 .297c-6.63 0-12 5.373-12 12 0 5.303 3.438 9.8 8.205 11.385.6.113.82-.258.82-.577 0-.285-.01-1.04-.015-2.04-3.338.724-4.042-1.61-4.042-1.61C4.422 18.07 3.633 17.7 3.633 17.7c-1.087-.744.084-.729.084-.729 1.205.084 1.838 1.236 1.838 1.236 1.07 1.835 2.809 1.305 3.495.998.108-.776.417-1.305.76-1.605-2.665-.3-5.466-1.332-5.466-5.93 0-1.31.465-2.38 1.235-3.22-.135-.303-.54-1.523.105-3.176 0 0 1.005-.322 3.3 1.23.96-.267 1.98-.399 3-.405 1.02.006 2.04.138 3 .405 2.28-1.552 3.285-1.23 3.285-1.23.645 1.653.24 2.873.12 3.176.765.84 1.23 1.91 1.23 3.22 0 4.61-2.805 5.625-5.475 5.92.42.36.81 1.096.81 2.22 0 1.606-.015 2.896-.015 3.286 0 .315.21.69.825.57C20.565 22.092 24 17.592 24 12.297c0-6.627-5.373-12-12-12" />
    </svg>
  );
}

/** Share-intent targets — plain web intents, no SDKs, no trackers. */
function shareTargets(url: string, title: string) {
  const u = encodeURIComponent(url);
  const t = encodeURIComponent(`${title} — Stella docs`);
  return [
    { label: "Post to X", href: `https://x.com/intent/post?url=${u}&text=${t}` },
    { label: "Share on LinkedIn", href: `https://www.linkedin.com/sharing/share-offsite/?url=${u}` },
    { label: "Share on Reddit", href: `https://www.reddit.com/submit?url=${u}&title=${t}` },
    { label: "Share on Hacker News", href: `https://news.ycombinator.com/submitlink?u=${u}&t=${t}` },
  ];
}

function ShareMenu({ path, title }: { path: string; title: string }) {
  const [open, setOpen] = useState(false);
  const [copied, setCopied] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);

  // Close on any outside click — the usual dropdown contract.
  useEffect(() => {
    if (!open) return;
    const onClick = (e: MouseEvent) => {
      if (!rootRef.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onClick);
    return () => document.removeEventListener("mousedown", onClick);
  }, [open]);

  // The canonical URL is computed at click time from the real origin, so
  // previews, localhost, and production all share the right link.
  const absoluteUrl = () =>
    (typeof window === "undefined" ? "https://stella.oxagen.sh" : window.location.origin) + path;

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(absoluteUrl());
      setCopied(true);
      setTimeout(() => setCopied(false), 1600);
    } catch {
      /* clipboard unavailable — the intent links still work */
    }
  };

  return (
    <div ref={rootRef} className="relative">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-haspopup="menu"
        aria-expanded={open}
        className="inline-flex items-center gap-1.5 rounded-lg border border-fd-border bg-fd-card px-2.5 py-1.5 text-xs font-medium text-fd-muted-foreground transition-colors hover:text-fd-foreground"
      >
        <Share2 className="size-3.5" aria-hidden />
        Share
      </button>
      {open && (
        <div
          role="menu"
          className="absolute bottom-full right-0 z-20 mb-2 w-52 rounded-lg border border-fd-border bg-fd-popover p-1 shadow-lg"
        >
          {shareTargets(absoluteUrl(), title).map((target) => (
            <a
              key={target.label}
              role="menuitem"
              href={target.href}
              target="_blank"
              rel="noopener noreferrer"
              onClick={() => setOpen(false)}
              className="block rounded-md px-2.5 py-1.5 text-xs text-fd-popover-foreground hover:bg-fd-accent"
            >
              {target.label}
            </a>
          ))}
          <button
            type="button"
            role="menuitem"
            onClick={copy}
            className="flex w-full items-center gap-1.5 rounded-md px-2.5 py-1.5 text-left text-xs text-fd-popover-foreground hover:bg-fd-accent"
          >
            {copied ? <Check className="size-3.5" aria-hidden /> : <Copy className="size-3.5" aria-hidden />}
            {copied ? "Copied" : "Copy link"}
          </button>
        </div>
      )}
    </div>
  );
}

export function PageFooter({ path, title }: { path: string; title: string }) {
  return (
    <footer className="mt-12 border-t border-fd-border pt-5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-2">
          <a
            href={REPO_URL}
            target="_blank"
            rel="noopener noreferrer"
            className="inline-flex items-center gap-1.5 rounded-lg bg-neutral-900 px-2.5 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-85 dark:bg-white dark:text-neutral-900"
          >
            <GitHubMark className="size-3.5" />
            macanderson/stella
          </a>
          <a
            href={SPONSOR_URL}
            target="_blank"
            rel="noopener noreferrer"
            className="inline-flex items-center gap-1.5 rounded-lg border border-fd-border bg-fd-card px-2.5 py-1.5 text-xs font-medium text-fd-muted-foreground transition-colors hover:text-fd-foreground"
          >
            <Heart className="size-3.5 text-pink-500" aria-hidden />
            Sponsor
          </a>
        </div>
        <ShareMenu path={path} title={title} />
      </div>
      <p className="mt-4 text-xs leading-relaxed text-fd-muted-foreground">
        Stella is <strong className="font-semibold text-fd-foreground">totally free and open source</strong>,
        dual-licensed <strong className="font-semibold text-fd-foreground">MIT or Apache&nbsp;2.0</strong> — your
        choice: MIT&apos;s three-paragraph simplicity, or Apache&nbsp;2.0&apos;s explicit patent grant that
        enterprises prefer. Use it, fork it, embed it, ship it — no account and no Community/default telemetry
        egress. An explicitly enrolled Oxagen Enterprise managed seat has one signed, content-free operational
        exception. {" "}
        <a
          href="/docs/telemetry#oxagen-enterprise-managed-export"
          className="underline underline-offset-2 hover:text-fd-foreground"
        >
          Enterprise telemetry boundary →
        </a>{" "}
        <a href="/docs/donate" className="underline underline-offset-2 hover:text-fd-foreground">
          What that means →
        </a>
      </p>
    </footer>
  );
}
