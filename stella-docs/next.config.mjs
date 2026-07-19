import { createMDX } from "fumadocs-mdx/next";

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const nextConfig = {
  reactStrictMode: true,
  // The site is fully static (MDX + generateStaticParams); no image
  // optimization server is needed.
  images: {
    unoptimized: true,
  },
  // Agent Modes was consolidated from a section (index + goal-mode) into a
  // single page; keep the old deep link alive for bookmarks and search hits.
  async redirects() {
    return [
      {
        source: "/docs/agent-modes/goal-mode",
        destination: "/docs/agent-modes#outcome-driven-goal-mode",
        permanent: true,
      },
    ];
  },
};

export default withMDX(nextConfig);
